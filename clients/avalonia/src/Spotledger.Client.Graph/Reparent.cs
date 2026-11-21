namespace Spotledger.Client.Graph;

/// <summary>One requested re-parent: move <paramref name="Node"/> under
/// <paramref name="Target"/> (null moves it to the top level).</summary>
/// <param name="Node">The node being moved.</param>
/// <param name="Target">The new parent, or null for the top level.</param>
public readonly record struct ReparentRequest(GraphNodeKey Node, GraphNodeKey? Target);

/// <summary>The server's answer to "may this node move there?". Cycle validity is decided
/// by the server — the graph query sees the whole edge set, the view sees one page — so a
/// drop is refused here and never by a client-side ancestor walk.</summary>
/// <param name="Allowed">Whether the server permits the move.</param>
/// <param name="Reason">The plain-language reason when refused (null when allowed).</param>
public sealed record ReparentDecision(bool Allowed, string? Reason = null)
{
    /// <summary>The permitted answer.</summary>
    public static ReparentDecision Allow { get; } = new(true);

    /// <summary>A refusal carrying the server's reason.</summary>
    public static ReparentDecision Refuse(string reason) => new(false, reason);
}

/// <summary>What a completed re-parent changed, as the undo stack records it.</summary>
/// <param name="Node">The moved node.</param>
/// <param name="PreviousParent">The parent before the move (null for the top level).</param>
/// <param name="NewParent">The parent after the move (null for the top level).</param>
public sealed record ReparentChange(GraphNodeKey Node, GraphNodeKey? PreviousParent, GraphNodeKey? NewParent);

/// <summary>The outcome of a drop or paste: refused with the server's reason, or applied.</summary>
/// <param name="Change">The applied change, or null when refused.</param>
/// <param name="RefusalReason">The server's reason when refused, or null.</param>
public sealed record ReparentOutcome(ReparentChange? Change, string? RefusalReason)
{
    /// <summary>True when the server applied the move.</summary>
    public bool Succeeded => Change is not null;

    /// <summary>An applied outcome.</summary>
    public static ReparentOutcome Applied(ReparentChange change) => new(change, null);

    /// <summary>A refused outcome.</summary>
    public static ReparentOutcome Refused(string reason) => new(null, reason);
}

/// <summary>
/// The editing seam behind drag re-parenting. Both calls are document updates through the
/// typed client: <see cref="CheckAsync"/> is the drop-validity question the server answers
/// while the drag hovers, and <see cref="ApplyAsync"/> performs the update (and re-checks it,
/// because the server's answer can go stale mid-drag).
/// </summary>
public interface IHierarchyEditor
{
    /// <summary>Asks the server whether the move is valid.</summary>
    /// <param name="request">The requested move.</param>
    /// <param name="cancellationToken">Cancels the question.</param>
    Task<ReparentDecision> CheckAsync(ReparentRequest request, CancellationToken cancellationToken);

    /// <summary>Applies the move as a normal document update.</summary>
    /// <param name="request">The requested move.</param>
    /// <param name="cancellationToken">Cancels the update.</param>
    /// <returns>The change; throws <see cref="HierarchyEditRefusedException"/> when refused.</returns>
    Task<ReparentChange> ApplyAsync(ReparentRequest request, CancellationToken cancellationToken);
}

/// <summary>A re-parent the server refused, carrying its reason for the drop indicator.</summary>
public sealed class HierarchyEditRefusedException : Exception
{
    /// <summary>Creates the exception with the server's reason.</summary>
    public HierarchyEditRefusedException(string reason) : base(reason) => Reason = reason;

    /// <summary>Creates the exception with the server's reason and an inner failure.</summary>
    public HierarchyEditRefusedException(string reason, Exception innerException) : base(reason, innerException)
        => Reason = reason;

    /// <summary>Creates the exception.</summary>
    public HierarchyEditRefusedException() => Reason = "The move was refused.";

    /// <summary>The server's plain-language reason.</summary>
    public string Reason { get; }
}
