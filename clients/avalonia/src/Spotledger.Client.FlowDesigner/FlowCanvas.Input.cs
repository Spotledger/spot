using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;

namespace Spotledger.Client.FlowDesigner;

/// The input half of the canvas: the pointer gestures (pan, select, drag, connect, zoom) and the
/// complete keyboard map. Every keyboard action has a visible equivalent on the pointer, and every
/// pointer action has a key, so an analyst can build a whole process without a mouse.
public partial class FlowCanvas
{

    protected override void OnKeyDown(KeyEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        if (HandleKey(e.Key, e.KeyModifiers))
        {
            e.Handled = true;
            return;
        }

        base.OnKeyDown(e);
    }

    public bool HandleKey(Key key, KeyModifiers modifiers)
    {
        if (IsTypeAheadOpen)
        {
            return HandleTypeAheadKey(key);
        }

        bool control = (modifiers & KeyModifiers.Control) != 0;
        bool shift = (modifiers & KeyModifiers.Shift) != 0;

        if (control)
        {
            switch (key)
            {
                case Key.Z:
                    return shift ? Redo() : Undo();
                case Key.Y:
                    return Redo();
                case Key.C:
                    return CopySelection();
                case Key.V:
                    return PasteClipboard();
                case Key.X:
                    return CopySelection() && DeleteSelection();
                case Key.A:
                    SelectOnly(Document?.Nodes.Select(node => node.Id) ?? []);
                    return true;
                case Key.L:
                    return ApplyAutoLayout();
                default:
                    break;
            }
        }

        if (ConnectFrom is not null)
        {
            switch (key)
            {
                case Key.Tab when shift:
                case Key.Left:
                case Key.Up:
                    NextCandidate(-1);
                    return true;
                case Key.Tab:
                case Key.Right:
                case Key.Down:
                    NextCandidate(1);
                    return true;
                case Key.Enter:
                    return CompleteConnect(ConnectCandidate, out _);
                case Key.Escape:
                    CancelConnect();
                    return true;
                default:
                    break;
            }
        }

        switch (key)
        {
            case Key.Tab:
                MoveFocus(shift ? -1 : 1);
                return true;
            case Key.Escape:
                if (!Selection.IsEmpty)
                {
                    ClearSelection();
                    StatusMessage = null;
                    return true;
                }

                return false;
            case Key.Delete:
            case Key.Back:
                return DeleteSelection();
            case Key.Enter:
                if (Selection.IsEmpty)
                {
                    return false;
                }

                FocusRequested?.Invoke(this, new FlowFocusRequestedEventArgs("properties"));
                return true;
            case Key.A when !control:
                BeginTypeAhead();
                return true;
            case Key.C when !control:
                if (Selection.SingleNodeId is { } from)
                {
                    BeginConnect(from);
                    return true;
                }

                return false;
            case Key.Left:
            case Key.Right:
            case Key.Up:
            case Key.Down:
                return HandleArrow(key, shift, control);
            default:
                return false;
        }
    }

    public void BeginTypeAhead()
    {
        if (IsReadOnly)
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return;
        }

        IsTypeAheadOpen = true;
        TypeAheadQuery = string.Empty;
        StatusMessage = FlowStrings.PaletteHint;
        InvalidateVisual();
    }

    public void TypeTypeAhead(string text)
    {
        if (!IsTypeAheadOpen)
        {
            BeginTypeAhead();
        }

        if (!IsReadOnly)
        {
            TypeAheadQuery += text ?? string.Empty;
            InvalidateVisual();
        }
    }

    public IReadOnlyList<FlowKindSchema> TypeAheadMatches => Catalogue.TypeAhead(TypeAheadQuery);

    public bool CommitTypeAhead()
    {
        IReadOnlyList<FlowKindSchema> matches = TypeAheadMatches;
        if (matches.Count == 0)
        {
            StatusMessage = FlowStrings.KindMissing;
            CancelTypeAhead();
            return false;
        }

        IFlowCommand? command = AddNode(matches[0].Kind);
        CancelTypeAhead();
        return command is not null;
    }

    public void CancelTypeAhead()
    {
        IsTypeAheadOpen = false;
        TypeAheadQuery = string.Empty;
        InvalidateVisual();
    }

    protected override void OnPointerPressed(PointerPressedEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        base.OnPointerPressed(e);
        Focus();
        PointerPoint point = e.GetCurrentPoint(this);
        Point position = point.Position;
        bool middle = point.Properties.IsMiddleButtonPressed;
        bool control = (e.KeyModifiers & KeyModifiers.Control) != 0;
        bool shift = (e.KeyModifiers & KeyModifiers.Shift) != 0;

        if (MinimapRect.Contains(position) && Document is not null)
        {
            JumpFromMinimap(position);
            e.Handled = true;
            return;
        }

        if (middle || (point.Properties.IsLeftButtonPressed && HitTest(position) is null && PortHit(position, out _) is null))
        {
            _panning = true;
            _panStartScreen = position;
            e.Pointer.Capture(this);
            e.Handled = true;
            return;
        }

        if (!point.Properties.IsLeftButtonPressed)
        {
            return;
        }

        FlowNode? portNode = PortHit(position, out bool fromPort);
        if (portNode is not null && fromPort && ConnectFrom is null)
        {
            BeginConnect(portNode.Id);
            e.Pointer.Capture(this);
            e.Handled = true;
            return;
        }

        FlowNode? node = HitTest(position);
        if (node is null)
        {
            if (ConnectFrom is not null)
            {
                CancelConnect();
            }
            else
            {
                ClearSelection();
            }

            e.Handled = true;
            return;
        }

        if (control)
        {
            Selection.Toggle(node.Id);
            FocusedNodeId = node.Id;
            OnSelectionChanged();
        }
        else if (shift)
        {
            Selection.Add(node.Id);
            FocusedNodeId = node.Id;
            OnSelectionChanged();
        }
        else if (!Selection.Contains(node.Id))
        {
            Select(node.Id);
        }

        if (ConnectFrom is not null)
        {
            CompleteConnect(node.Id, out _);
            e.Handled = true;
            return;
        }

        _dragging = true;
        _dragMoved = false;
        _dragStartScreen = position;
        _dragStartWorld = ToWorld(position);
        _dragOrigins.Clear();
        foreach (string id in Selection.NodeIds)
        {
            if (Document?.Node(id) is { } selected)
            {
                _dragOrigins[id] = selected.Position;
            }
        }

        e.Pointer.Capture(this);
        e.Handled = true;
    }

    protected override void OnPointerMoved(PointerEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        base.OnPointerMoved(e);
        Point position = e.GetPosition(this);

        if (_panning)
        {
            PanBy(position.X - _panStartScreen.X, position.Y - _panStartScreen.Y);
            _panStartScreen = position;
            return;
        }

        if (ConnectFrom is not null)
        {
            ConnectPointer = ToWorld(position);
            FlowNode? over = HitTest(position);
            if (over is not null && !string.Equals(over.Id, ConnectFrom, StringComparison.Ordinal))
            {
                ConnectCandidate = over.Id;
            }

            InvalidateVisual();
            return;
        }

        if (!_dragging || Document is null)
        {
            return;
        }

        FlowPoint world = ToWorld(position);
        double dx = world.X - _dragStartWorld.X;
        double dy = world.Y - _dragStartWorld.Y;
        if (Math.Abs(dx) < 1 && Math.Abs(dy) < 1)
        {
            return;
        }

        _dragMoved = true;
        foreach (KeyValuePair<string, FlowPoint> pair in _dragOrigins)
        {
            if (Document.Node(pair.Key) is { } node)
            {
                FlowPoint moved = pair.Value.Offset(dx, dy);
                node.Position = SnapToGrid ? FlowLayoutEngine.Snap(moved) : moved;
            }
        }

        InvalidateVisual();
    }

    protected override void OnPointerReleased(PointerReleasedEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        base.OnPointerReleased(e);
        Point position = e.GetPosition(this);

        if (_panning)
        {
            _panning = false;
            e.Pointer.Capture(null);
            return;
        }

        if (ConnectFrom is not null)
        {
            FlowNode? target = HitTest(position);
            if (target is not null)
            {
                CompleteConnect(target.Id, out _);
            }
            else
            {
                CancelConnect();
            }

            e.Pointer.Capture(null);
            return;
        }

        if (!_dragging)
        {
            return;
        }

        _dragging = false;
        e.Pointer.Capture(null);
        if (!_dragMoved || Document is null || Editor is null)
        {
            _dragOrigins.Clear();
            return;
        }

        FlowPoint end = ToWorld(position);
        double dx = end.X - _dragStartWorld.X;
        double dy = end.Y - _dragStartWorld.Y;
        if (SnapToGrid)
        {
            dx = FlowLayoutEngine.Snap(dx);
            dy = FlowLayoutEngine.Snap(dy);
        }

        // The drag previewed the move on the nodes; put them back so the command captures the
        // exact before/after pair and undo is exact.
        foreach (KeyValuePair<string, FlowPoint> pair in _dragOrigins)
        {
            if (Document.Node(pair.Key) is { } node)
            {
                node.Position = pair.Value;
            }
        }

        MoveNodesCommand command = new(Document, _dragOrigins.Keys, dx, dy);
        if (command.HasMovement)
        {
            Editor.Execute(command);
            StatusMessage = command.Label;
        }

        _dragOrigins.Clear();
        InvalidateVisual();
    }

    protected override void OnPointerWheelChanged(PointerWheelEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        base.OnPointerWheelChanged(e);
        double factor = Math.Pow(1.15, e.Delta.Y);
        SetZoom(Zoom * factor, e.GetPosition(this));
        e.Handled = true;
    }

    private bool HandleArrow(Key key, bool shift, bool control)
    {
        if (Selection.NodeIds.Count == 0)
        {
            int step = key is Key.Left or Key.Up ? -1 : 1;
            return MoveNodeFocus(step) is not null;
        }

        double step2 = control ? 1 : GridSize;
        if (shift)
        {
            step2 *= 10;
        }

        (double dx, double dy) = key switch
        {
            Key.Left => (-step2, 0d),
            Key.Right => (step2, 0d),
            Key.Up => (0d, -step2),
            _ => (0d, step2),
        };

        return MoveSelection(dx, dy);
    }

    private bool HandleTypeAheadKey(Key key)
    {
        if (key == Key.Escape)
        {
            CancelTypeAhead();
            StatusMessage = null;
            return true;
        }

        if (key == Key.Enter)
        {
            return CommitTypeAhead();
        }

        if (key == Key.Back)
        {
            if (TypeAheadQuery.Length > 0)
            {
                TypeAheadQuery = TypeAheadQuery[..^1];
                InvalidateVisual();
            }

            return true;
        }

        int letter = (int)key;
        if (letter >= (int)Key.A && letter <= (int)Key.Z)
        {
            TypeAheadQuery += ((char)('a' + (letter - (int)Key.A))).ToString();
            InvalidateVisual();
            return true;
        }

        return true;
    }

    public void ClickMinimap(Point position) => JumpFromMinimap(position);

    private void JumpFromMinimap(Point position)
    {
        if (Minimap is not { } projection || Document is null)
        {
            return;
        }

        FlowPoint world = projection.ToWorld(new FlowPoint(position.X - MinimapRect.X, position.Y - MinimapRect.Y));
        double centreX = world.X;
        double centreY = world.Y;
        _panX = (Bounds.Width / 2) - (centreX * Zoom);
        _panY = (Bounds.Height / 2) - (centreY * Zoom);
        InvalidateVisual();
        OnStatusChanged();
    }
}
