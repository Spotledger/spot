namespace Spotledger.Client.Graph;

/// <summary>
/// Drag-and-drop re-parenting and its keyboard alternative (cut then paste). The controller
/// asks the server whether a drop is valid while the pointer hovers and asks again when the
/// drop lands; it never decides validity from the pages the tree happens to hold. An applied
/// move is a normal document update through <see cref="IHierarchyEditor"/>, and its inverse
/// goes onto the workspace undo stack.
/// </summary>
public sealed class HierarchyReparentController
{
    private IWorkspaceUndo? _undo;
    private GraphNodeKey? _dragged;
    private GraphNodeKey? _cut;
    private GraphNodeKey? _hover;

    /// <summary>Creates the controller over an editing gateway.</summary>
    /// <param name="editor">The server-backed editor.</param>
    /// <param name="undo">The workspace undo stack (null disables undo registration).</param>
    public HierarchyReparentController(IHierarchyEditor editor, IWorkspaceUndo? undo = null)
    {
        ArgumentNullException.ThrowIfNull(editor);
        Editor = editor;
        _undo = undo;
    }

    /// <summary>The server-backed editor.</summary>
    public IHierarchyEditor Editor { get; }

    /// <summary>The node being dragged or cut, or null.</summary>
    public GraphNodeKey? Node => _dragged ?? _cut;

    /// <summary>True while a pointer drag is in progress.</summary>
    public bool IsDragging => _dragged is not null;

    /// <summary>True while a cut is waiting for a paste.</summary>
    public bool IsCutPending => _cut is not null;

    /// <summary>The row the pointer currently hovers.</summary>
    public GraphNodeKey? HoverTarget => _hover;

    /// <summary>The server's answer for the current hover, or null before it arrives.</summary>
    public ReparentDecision? HoverDecision { get; private set; }

    /// <summary>The last refusal reason, for the drop indicator's caption.</summary>
    public string? LastRefusal { get; private set; }

    /// <summary>Raised when the drag, cut or hover state changed.</summary>
    public event EventHandler? Changed;

    /// <summary>Replaces the undo stack (the control's Undo property can change at runtime).</summary>
    /// <param name="undo">The stack, or null.</param>
    public void SetUndo(IWorkspaceUndo? undo) => _undo = undo;

    /// <summary>Starts a pointer drag on a node.</summary>
    /// <param name="key">The node.</param>
    public void BeginDrag(GraphNodeKey key)
    {
        _dragged = key;
        _cut = null;
        _hover = null;
        HoverDecision = null;
        LastRefusal = null;
        RaiseChanged();
    }

    /// <summary>Moves the hover to a row, clearing the previous validity answer.</summary>
    /// <param name="target">The hovered node, or null when outside any row.</param>
    public void Hover(GraphNodeKey? target)
    {
        if (_hover == target)
        {
            return;
        }

        _hover = target;
        HoverDecision = null;
        RaiseChanged();
    }

    /// <summary>Asks the server whether the current hover would be accepted. A stale answer
    /// (the pointer moved on while the question was in flight) is discarded.</summary>
    /// <param name="cancellationToken">Cancels the question.</param>
    public async Task<ReparentDecision?> ResolveHoverAsync(CancellationToken cancellationToken)
    {
        if (Node is not { } node || _hover is not { } target)
        {
            HoverDecision = null;
            RaiseChanged();
            return null;
        }

        var asked = target;
        var decision = await Editor.CheckAsync(new ReparentRequest(node, asked), cancellationToken)
            .ConfigureAwait(true);
        if (_hover != asked)
        {
            return HoverDecision;
        }

        HoverDecision = decision;
        LastRefusal = decision.Allowed ? null : decision.Reason;
        RaiseChanged();
        return decision;
    }

    /// <summary>Drops the dragged node on a target: the server re-checks and applies the move.</summary>
    /// <param name="target">The drop target, or null for the top level.</param>
    /// <param name="cancellationToken">Cancels the update.</param>
    public async Task<ReparentOutcome> DropAsync(GraphNodeKey? target, CancellationToken cancellationToken)
    {
        if (_dragged is not { } node)
        {
            return ReparentOutcome.Refused("Nothing is being dragged.");
        }

        var outcome = await ApplyAsync(node, target, cancellationToken).ConfigureAwait(true);
        _dragged = null;
        _hover = null;
        HoverDecision = null;
        RaiseChanged();
        return outcome;
    }

    /// <summary>Marks a node for the keyboard alternative (cut).</summary>
    /// <param name="key">The node.</param>
    public void Cut(GraphNodeKey key)
    {
        _cut = key;
        _dragged = null;
        _hover = null;
        HoverDecision = null;
        LastRefusal = null;
        RaiseChanged();
    }

    /// <summary>Clears a pending cut (Escape).</summary>
    public void CancelCut()
    {
        if (_cut is null)
        {
            return;
        }

        _cut = null;
        LastRefusal = null;
        RaiseChanged();
    }

    /// <summary>Ends a drag without applying anything (Escape or a release outside a row).</summary>
    public void CancelDrag()
    {
        if (_dragged is null && _hover is null)
        {
            return;
        }

        _dragged = null;
        _hover = null;
        HoverDecision = null;
        RaiseChanged();
    }

    /// <summary>Pastes the cut node onto a target: the same server-checked document update.</summary>
    /// <param name="target">The new parent, or null for the top level.</param>
    /// <param name="cancellationToken">Cancels the update.</param>
    public async Task<ReparentOutcome> PasteAsync(GraphNodeKey? target, CancellationToken cancellationToken)
    {
        if (_cut is not { } node)
        {
            return ReparentOutcome.Refused("Nothing is cut.");
        }

        var outcome = await ApplyAsync(node, target, cancellationToken).ConfigureAwait(true);
        if (outcome.Succeeded)
        {
            _cut = null;
        }

        RaiseChanged();
        return outcome;
    }

    private async Task<ReparentOutcome> ApplyAsync(GraphNodeKey node, GraphNodeKey? target, CancellationToken cancellationToken)
    {
        if (target == node)
        {
            // Dropping a node on itself moves nothing; the server would refuse it, so no
            // request is sent. This is not a cycle decision — the server owns those.
            return ReparentOutcome.Refused("A node cannot be its own parent.");
        }

        try
        {
            var change = await Editor.ApplyAsync(new ReparentRequest(node, target), cancellationToken)
                .ConfigureAwait(true);
            LastRefusal = null;
            _undo?.Record(UndoableEdit.Reparent(change, Editor));
            return ReparentOutcome.Applied(change);
        }
        catch (HierarchyEditRefusedException refused)
        {
            LastRefusal = refused.Reason;
            return ReparentOutcome.Refused(refused.Reason);
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            LastRefusal = error.Message;
            return ReparentOutcome.Refused(error.Message);
        }
    }

    private void RaiseChanged() => Changed?.Invoke(this, EventArgs.Empty);
}
