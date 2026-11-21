using Avalonia;
using Avalonia.Input;
using Avalonia.Interactivity;

namespace Spotledger.Client.Graph;

/// <summary>Input half of <see cref="SpotHierarchy"/>: the keyboard table (arrows, Home/End,
/// PageUp/PageDown, Left/Right expand-collapse, <c>*</c> expand-siblings, type-ahead, select,
/// cut/paste) and the pointer table (click, chevron, double-click activation, wheel, drag with
/// a drop indicator). Keyboard work is synchronous over the loaded rows; only the page a
/// command reveals is fetched asynchronously.</summary>
public partial class SpotHierarchy
{
    private GraphNodeKey? _lastClickKey;
    private DateTimeOffset _lastClickStamp = DateTimeOffset.MinValue;

    internal const double ChevronWidth = 16;

    internal const double IndentWidth = 20;

    internal const double RowPadding = 6;

    /// <inheritdoc />
    protected override void OnKeyDown(KeyEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        if (_model is not { } model)
        {
            base.OnKeyDown(e);
            return;
        }

        var control = e.KeyModifiers.HasFlag(KeyModifiers.Control) || e.KeyModifiers.HasFlag(KeyModifiers.Meta);
        var shift = e.KeyModifiers.HasFlag(KeyModifiers.Shift);
        switch (e.Key)
        {
            case Key.Down:
                MoveCursor(model, 1, shift, control);
                break;
            case Key.Up:
                MoveCursor(model, -1, shift, control);
                break;
            case Key.PageDown:
                model.PageMove(Math.Max(1, VisibleRowCount()), forward: true);
                RevealCursor();
                break;
            case Key.PageUp:
                model.PageMove(Math.Max(1, VisibleRowCount()), forward: false);
                RevealCursor();
                break;
            case Key.Home:
                model.MoveToFirst();
                RevealCursor();
                break;
            case Key.End:
                model.MoveToLast();
                RevealCursor();
                break;
            case Key.Left when !IsRtl:
            case Key.Right when IsRtl:
                Start(CollapseOrMoveToParentAsync);
                break;
            case Key.Right when !IsRtl:
            case Key.Left when IsRtl:
                Start(ExpandOrMoveToFirstChildAsync);
                break;
            case Key.Multiply:
                Start(ExpandSiblingsAsync);
                break;
            case Key.Space:
                model.ToggleCursorSelection();
                NotifySelectionChanged();
                break;
            case Key.Enter:
                ActivateCursor();
                break;
            case Key.Escape:
                _reparent?.CancelDrag();
                _reparent?.CancelCut();
                model.ClearTypeAhead();
                InvalidateVisual();
                break;
            case Key.A when control:
                model.SelectAllVisible();
                NotifySelectionChanged();
                break;
            case Key.X when control:
                CutCursor();
                break;
            case Key.V when control:
                Start(PasteAsync);
                break;
            case Key.D8 when shift:
                Start(ExpandSiblingsAsync);
                break;
            default:
                if (!control && e.KeySymbol is { Length: > 0 } symbol)
                {
                    model.TypeAhead(symbol, Clock.Now);
                    RevealCursor();
                }
                else
                {
                    base.OnKeyDown(e);
                    return;
                }

                break;
        }

        InvalidateVisual();
        e.Handled = true;
    }

    /// <inheritdoc />
    protected override void OnPointerPressed(PointerPressedEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        var point = e.GetCurrentPoint(this);
        if (!point.Properties.IsLeftButtonPressed || _model is not { } model)
        {
            base.OnPointerPressed(e);
            return;
        }

        Focus();
        var index = RowIndexAt(point.Position.Y);
        if (index < 0 || index >= model.RowCount)
        {
            e.Handled = true;
            return;
        }

        var row = model.Rows[index];
        if (row.Node.HasChildren && IsChevronHit(row, point.Position.X))
        {
            Start(token => model.ToggleAsync(row.Key, token));
            InvalidateVisual();
            e.Handled = true;
            return;
        }

        var now = Clock.Now;
        var doubleClick = _lastClickKey == row.Key && (now - _lastClickStamp).TotalMilliseconds < 500;
        _lastClickKey = row.Key;
        _lastClickStamp = now;

        model.MoveCursorTo(row.Key);
        if (e.KeyModifiers.HasFlag(KeyModifiers.Control) || e.KeyModifiers.HasFlag(KeyModifiers.Meta))
        {
            model.Selection.Toggle(row.Key);
        }
        else if (e.KeyModifiers.HasFlag(KeyModifiers.Shift))
        {
            model.Selection.ExtendTo(model.Rows, row.Key);
        }
        else
        {
            model.Selection.Select(row.Key);
        }

        NotifySelectionChanged();
        InvalidateVisual();
        if (doubleClick)
        {
            Activate(row);
        }
        else if (Editor is not null)
        {
            _dragOrigin = row.Key;
            _dragStart = point.Position;
            _dragArmed = true;
            e.Pointer.Capture(this);
        }

        e.Handled = true;
    }

    /// <inheritdoc />
    protected override void OnPointerMoved(PointerEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        if (!_dragArmed || _reparent is not { } reparent || _dragOrigin is not { } origin)
        {
            base.OnPointerMoved(e);
            return;
        }

        var point = e.GetCurrentPoint(this);
        if (!point.Properties.IsLeftButtonPressed)
        {
            _dragArmed = false;
            return;
        }

        if (!reparent.IsDragging
            && (Math.Abs(point.Position.X - _dragStart.X) > 6 || Math.Abs(point.Position.Y - _dragStart.Y) > 6))
        {
            reparent.BeginDrag(origin);
        }

        if (!reparent.IsDragging)
        {
            return;
        }

        var index = RowIndexAt(point.Position.Y);
        var target = index >= 0 && _model is { } model ? model.Rows[index].Key : (GraphNodeKey?)null;
        reparent.Hover(target);
        Start(ResolveHoverAsync);
        InvalidateVisual();
        e.Handled = true;
    }

    /// <inheritdoc />
    protected override void OnPointerReleased(PointerReleasedEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        _dragArmed = false;
        if (_reparent is { IsDragging: true } reparent)
        {
            Start(DropAsync);
            InvalidateVisual();
            e.Pointer.Capture(null);
            e.Handled = true;
            return;
        }

        base.OnPointerReleased(e);
    }

    /// <inheritdoc />
    protected override void OnPointerWheelChanged(PointerWheelEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        ScrollBy(-e.Delta.Y * 3);
        e.Handled = true;
    }

    internal bool IsChevronHit(HierarchyRow row, double x)
    {
        ArgumentNullException.ThrowIfNull(row);
        var start = ChevronLeft(row.Depth);
        return x >= start && x <= start + ChevronWidth;
    }

    internal double ChevronLeft(int depth)
        => IsRtl
            ? Math.Max(0, Bounds.Width - RowPadding - ((depth + 1) * IndentWidth))
            : RowPadding + (depth * IndentWidth);

    private static void MoveCursor(HierarchyModel model, int delta, bool shift, bool control)
    {
        model.MoveCursor(delta);
        if (control)
        {
            return;
        }

        model.SelectCursor(shift);
    }

    private async Task CollapseOrMoveToParentAsync(CancellationToken cancellationToken)
    {
        if (_model is { } model)
        {
            await model.CollapseOrMoveToParentAsync(cancellationToken).ConfigureAwait(true);
            RevealCursor();
        }
    }

    private async Task ExpandOrMoveToFirstChildAsync(CancellationToken cancellationToken)
    {
        if (_model is { } model)
        {
            await model.ExpandOrMoveToFirstChildAsync(cancellationToken).ConfigureAwait(true);
            RevealCursor();
        }
    }

    private async Task ExpandSiblingsAsync(CancellationToken cancellationToken)
    {
        if (_model is { } model && await model.ExpandSiblingsOfCursorAsync(cancellationToken).ConfigureAwait(true))
        {
            InvalidateVisual();
        }
    }

    private async Task ResolveHoverAsync(CancellationToken cancellationToken)
    {
        if (_reparent is { } reparent)
        {
            await reparent.ResolveHoverAsync(cancellationToken).ConfigureAwait(true);
        }
    }

    private async Task DropAsync(CancellationToken cancellationToken)
    {
        if (_reparent is not { } reparent)
        {
            return;
        }

        var target = reparent.HoverTarget;
        if (target is null)
        {
            // A release outside every row (the breadcrumb band, empty space, off the control)
            // is a cancelled drag, never an implicit move to the top level.
            reparent.CancelDrag();
            return;
        }

        var outcome = await reparent.DropAsync(target, cancellationToken).ConfigureAwait(true);
        if (outcome.Succeeded && _model is { } model)
        {
            await model.ReloadAsync(cancellationToken).ConfigureAwait(true);
        }
    }

    private async Task PasteAsync(CancellationToken cancellationToken)
    {
        if (_reparent is not { } reparent || _model is not { } model || model.Cursor is not { } target)
        {
            return;
        }

        var outcome = await reparent.PasteAsync(target, cancellationToken).ConfigureAwait(true);
        if (outcome.Succeeded)
        {
            await model.ReloadAsync(cancellationToken).ConfigureAwait(true);
        }
    }

    private void CutCursor()
    {
        if (_reparent is { } reparent && _model is { Cursor: { } key })
        {
            reparent.Cut(key);
            InvalidateVisual();
        }
    }

    private void ActivateCursor()
    {
        if (_model is not { } model || model.CursorIndex is var index && index < 0)
        {
            return;
        }

        Activate(model.Rows[index]);
    }

    private void Activate(HierarchyRow row)
    {
        RowActivated?.Invoke(this, new HierarchyRowActivatedEventArgs(row, _model?.IndexOf(row.Key) ?? -1));
        if (Navigation is { } navigation)
        {
            Start(token => navigation.OpenDocumentAsync(row.Key, token));
        }
    }
}
