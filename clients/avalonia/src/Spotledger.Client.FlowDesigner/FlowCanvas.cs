using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;

namespace Spotledger.Client.FlowDesigner;

/// The answer to "may this connection be made?", as a local UX hint.
public sealed record FlowConnectionFeedback(bool Valid, string? Reason = null)
{

    public static FlowConnectionFeedback Allowed { get; } = new(true);

    public static FlowConnectionFeedback Refused(string reason) => new(false, reason);
}

/// Which panel the tab ring moved to.
public sealed class FlowFocusRequestedEventArgs(string panel) : EventArgs
{

    public string Panel { get; } = panel;
}

/// The process canvas: a pan-and-zoom drawing of the document with grid snapping, drag from the
/// palette, port-to-port connections with live feedback, multi-select, copy and paste, a minimap,
/// and complete keyboard editing. It owns presentation and the editing gestures only — every edit
/// goes through <see cref="FlowEditor"/> (so undo is exact) and every question of validity beyond
/// the local hints goes to the server through <see cref="Validation"/>. Properties are plain CLR
/// properties: the drawing is hand-painted, so there is nothing for a control theme to bind.
public partial class FlowCanvas : Control
{

    private FlowEditor? _editor;

    private double _zoom = 1;

    private double _panX;

    private double _panY;

    private string? _statusMessage;

    private bool _panning;

    private bool _dragging;

    private Point _panStartScreen;

    private Point _dragStartScreen;

    private FlowPoint _dragStartWorld;

    private bool _dragMoved;

    private readonly Dictionary<string, FlowPoint> _dragOrigins = new(StringComparer.Ordinal);

    public FlowCanvas()
    {
        Focusable = true;
        ClipToBounds = true;
        AutomationProperties.SetAutomationId(this, "flow-canvas");
        AutomationProperties.SetName(this, FlowStrings.CanvasName);
        AutomationProperties.SetHelpText(this, FlowStrings.EmptyCanvas);
    }

    public static IReadOnlyList<string> PanelOrder { get; } = ["canvas", "palette", "properties", "problems"];

    public FlowDocument? Document
    {
        get => Editor?.Document;
        set
        {
            if (ReferenceEquals(Document, value))
            {
                return;
            }

            _editor = value is null ? null : new FlowEditor(value);
            if (_editor is not null)
            {
                _editor.Changed += (_, _) => OnDocumentChanged();
            }

            Selection.Clear();
            ConnectFrom = null;
            CancelTypeAhead();
            if (value is not null && value.Nodes.Count > 0 && value.Nodes.All(node => node.Position == default))
            {
                FlowLayoutEngine.Apply(value, FlowLayoutEngine.Compute(value));
            }

            InvalidateVisual();
            InvalidateMeasure();
            OnDocumentChanged();
        }
    }

    public FlowEditor? Editor
    {
        get => _editor;
        set
        {
            if (ReferenceEquals(_editor, value))
            {
                return;
            }

            _editor = value;
            if (_editor is not null)
            {
                _editor.Changed += (_, _) => OnDocumentChanged();
            }

            Selection.Clear();
            InvalidateVisual();
            OnDocumentChanged();
        }
    }

    public FlowKindCatalogue Catalogue { get; set; } = FlowKindCatalogue.Empty;

    public FlowValidationCoordinator? Validation { get; set; }

    public FlowStrings Strings { get; set; } = new();

    public FlowSelection Selection { get; } = new();

    public FlowClipboard Clipboard { get; } = new();

    public bool IsReadOnly
    {
        get => Editor is { CanEdit: false } || Editor is null;
        set
        {
            if (Editor is not null)
            {
                Editor.CanEdit = !value;
            }

            InvalidateVisual();
            OnStatusChanged();
        }
    }

    public bool SnapToGrid { get; set; } = true;

    public double Zoom
    {
        get => _zoom;
        set
        {
            SetZoom(value, new Point(Bounds.Width / 2, Bounds.Height / 2));
        }
    }

    public double PanX
    {
        get => _panX;
        private set
        {
            if (Math.Abs(_panX - value) > double.Epsilon)
            {
                _panX = value;
                InvalidateVisual();
            }
        }
    }

    public double PanY
    {
        get => _panY;
        private set
        {
            if (Math.Abs(_panY - value) > double.Epsilon)
            {
                _panY = value;
                InvalidateVisual();
            }
        }
    }

    public static double GridSize => FlowLayoutEngine.GridSize;

    public string? ConnectFrom { get; private set; }

    public string? ConnectCandidate { get; private set; }

    public FlowPoint? ConnectPointer { get; private set; }

    public string? StatusMessage
    {
        get => _statusMessage;
        private set
        {
            if (!string.Equals(_statusMessage, value, StringComparison.Ordinal))
            {
                _statusMessage = value;
                OnStatusChanged();
            }
        }
    }

    public string TypeAheadQuery { get; private set; } = string.Empty;

    public bool IsTypeAheadOpen { get; private set; }

    public string FocusedPanel => PanelOrder[FocusedPanelIndex];

    public int FocusedPanelIndex { get; private set; }

    public string? FocusedNodeId { get; private set; }

    public IReadOnlyList<FlowProblem> Problems => Validation?.Problems ?? [];

    public IReadOnlyList<FlowProblemRow> ProblemRows => FlowProblemList.Rows(Problems);

    public string? PaletteDragKind { get; private set; }

    public MinimapProjection? Minimap =>
        Document is null ? null : MinimapProjection.For(Document, MinimapWidth, MinimapHeight);

    public static double MinimapWidth => 192;

    public static double MinimapHeight => 128;

    public Rect MinimapRect =>
        new(Math.Max(8, Bounds.Width - MinimapWidth - 16), Math.Max(8, Bounds.Height - MinimapHeight - 16), MinimapWidth, MinimapHeight);

    public event EventHandler? SelectionChanged;

    public event EventHandler? DocumentChanged;

    public event EventHandler? StatusChanged;

    public event EventHandler<FlowFocusRequestedEventArgs>? FocusRequested;

    public Point ToScreen(FlowPoint world) =>
        new((world.X * Zoom) + PanX, (world.Y * Zoom) + PanY);

    public FlowPoint ToWorld(Point screen) =>
        new((screen.X - PanX) / Zoom, (screen.Y - PanY) / Zoom);

    public static Rect WorldBounds(FlowNode node)
    {
        ArgumentNullException.ThrowIfNull(node);
        return new Rect(node.Position.X, node.Position.Y, FlowLayoutEngine.NodeWidth, FlowLayoutEngine.NodeHeight);
    }

    public static FlowPoint OutputPort(FlowNode node)
    {
        ArgumentNullException.ThrowIfNull(node);
        return new FlowPoint(node.Position.X + FlowLayoutEngine.NodeWidth, node.Position.Y + (FlowLayoutEngine.NodeHeight / 2));
    }

    public static FlowPoint InputPort(FlowNode node)
    {
        ArgumentNullException.ThrowIfNull(node);
        return new FlowPoint(node.Position.X, node.Position.Y + (FlowLayoutEngine.NodeHeight / 2));
    }

    public FlowNode? HitTest(Point screen)
    {
        if (Document is null)
        {
            return null;
        }

        FlowPoint world = ToWorld(screen);
        for (int i = Document.Nodes.Count - 1; i >= 0; i--)
        {
            FlowNode node = Document.Nodes[i];
            if (WorldBounds(node).Contains(new Point(world.X, world.Y)))
            {
                return node;
            }
        }

        return null;
    }

    public FlowNode? PortHit(Point screen, out bool fromPort)
    {
        fromPort = false;
        if (Document is null)
        {
            return null;
        }

        double radius = Math.Max(6, 10 * Zoom);
        foreach (FlowNode node in Document.Nodes)
        {
            if (Distance(screen, ToScreen(OutputPort(node))) <= radius)
            {
                fromPort = true;
                return node;
            }

            if (Distance(screen, ToScreen(InputPort(node))) <= radius)
            {
                fromPort = false;
                return node;
            }
        }

        return null;
    }

    public void PanBy(double dx, double dy)
    {
        PanX += dx;
        PanY += dy;
    }

    public void SetZoom(double zoom, Point anchor)
    {
        double clamped = Math.Clamp(zoom, 0.25, 3);
        if (Math.Abs(clamped - _zoom) < 1e-9)
        {
            return;
        }

        FlowPoint before = ToWorld(anchor);
        _zoom = clamped;
        _panX = anchor.X - (before.X * _zoom);
        _panY = anchor.Y - (before.Y * _zoom);
        InvalidateVisual();
        OnStatusChanged();
    }

    public void FitToContent()
    {
        if (Document is null || Document.Nodes.Count == 0 || Bounds.Width <= 0 || Bounds.Height <= 0)
        {
            return;
        }

        double width = Document.Nodes.Max(node => node.Position.X + FlowLayoutEngine.NodeWidth);
        double height = Document.Nodes.Max(node => node.Position.Y + FlowLayoutEngine.NodeHeight);
        double zoom = Math.Clamp(
            Math.Min((Bounds.Width - 32) / Math.Max(width, 1), (Bounds.Height - 32) / Math.Max(height, 1)),
            0.25,
            3);
        _zoom = zoom;
        _panX = (Bounds.Width - (width * zoom)) / 2;
        _panY = (Bounds.Height - (height * zoom)) / 2;
        InvalidateVisual();
        OnStatusChanged();
    }

    public void Select(string id)
    {
        Selection.SelectOnly(id);
        FocusedNodeId = id;
        OnSelectionChanged();
    }

    public void SelectOnly(IEnumerable<string> ids)
    {
        Selection.SelectOnly(ids);
        FocusedNodeId = LastSelectedNode();
        OnSelectionChanged();
    }

    public bool ToggleSelection(string id)
    {
        bool selected = Selection.Toggle(id);
        FocusedNodeId = id;
        OnSelectionChanged();
        return selected;
    }

    public void ClearSelection()
    {
        Selection.Clear();
        OnSelectionChanged();
    }

    public bool FocusNode(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        FlowNode? node = Document?.Node(id);
        if (node is null)
        {
            StatusMessage = FlowStrings.KindMissing;
            return false;
        }

        Select(id);
        Point centre = ToScreen(new FlowPoint(
            node.Position.X + (FlowLayoutEngine.NodeWidth / 2),
            node.Position.Y + (FlowLayoutEngine.NodeHeight / 2)));
        PanBy((Bounds.Width / 2) - centre.X, (Bounds.Height / 2) - centre.Y);
        return true;
    }

    public IFlowCommand? AddNode(string kind, string? id = null, FlowPoint? at = null)
    {
        ArgumentNullException.ThrowIfNull(kind);
        if (Document is null || Editor is null)
        {
            return null;
        }

        if (!Editor.CanEdit)
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return null;
        }

        FlowKindSchema? schema = Catalogue.Schema(kind);
        if (schema is null)
        {
            StatusMessage = FlowStrings.KindMissing;
            return null;
        }

        string wanted = id ?? FlowId.Sanitize(kind);
        string free = Document.NextFreeId(wanted);
        FlowNode node = schema.NewNode(free);
        FlowPoint position = at ?? ToWorld(new Point(Bounds.Width / 2, Bounds.Height / 2));
        node.Position = SnapToGrid ? FlowLayoutEngine.Snap(position) : position;
        AddNodeCommand command = new(node);
        if (!Editor.Execute(command))
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return null;
        }

        Selection.SelectOnly(free);
        FocusedNodeId = free;
        StatusMessage = FlowStrings.NodeName(schema.DisplayName, free);
        OnSelectionChanged();
        return command;
    }

    public bool BeginPaletteDrag(string kind)
    {
        ArgumentNullException.ThrowIfNull(kind);
        if (IsReadOnly || Catalogue.Schema(kind) is null)
        {
            StatusMessage = IsReadOnly ? FlowStrings.RefusedReadOnly : FlowStrings.KindMissing;
            return false;
        }

        PaletteDragKind = kind;
        StatusMessage = FlowStrings.NodeName(Catalogue.DisplayName(kind), string.Empty);
        return true;
    }

    public IFlowCommand? DropFromPalette(Point screen)
    {
        if (PaletteDragKind is not { } kind)
        {
            return null;
        }

        PaletteDragKind = null;
        FlowPoint world = ToWorld(screen);
        return AddNode(kind, null, world);
    }

    public void CancelPaletteDrag()
    {
        PaletteDragKind = null;
        StatusMessage = null;
    }

    public bool FocusProblem(FlowProblem? problem)
    {
        if (problem is null)
        {
            return false;
        }

        if (problem.NodeId is { } nodeId)
        {
            return FocusNode(nodeId);
        }

        if (problem.Edge is { } edge && Document?.Node(edge.From) is not null)
        {
            Selection.Clear();
            Selection.ToggleEdge(edge);
            StatusMessage = FlowStrings.EdgeName(edge.From, edge.To);
            OnSelectionChanged();
            return true;
        }

        return false;
    }

    public bool DeleteSelection()
    {
        if (Editor is null || Selection.IsEmpty)
        {
            return false;
        }

        if (!Editor.CanEdit)
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return false;
        }

        DeleteSelectionCommand command = new(Selection.NodeIds, Selection.EdgeKeys);
        if (!Editor.Execute(command))
        {
            return false;
        }

        Selection.Clear();
        FocusedNodeId = null;
        StatusMessage = command.Label;
        OnSelectionChanged();
        return true;
    }

    public bool CopySelection()
    {
        if (Document is null || Selection.NodeIds.Count == 0)
        {
            return false;
        }

        Clipboard.Copy(Document, Selection);
        StatusMessage = FlowStrings.Copy + " " + FlowStrings.Selected(Clipboard.NodeCount);
        return true;
    }

    public bool PasteClipboard()
    {
        if (Document is null || Editor is null)
        {
            return false;
        }

        if (!Editor.CanEdit)
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return false;
        }

        PasteCommand? command = Clipboard.PreparePaste(Document);
        if (command is null)
        {
            return false;
        }

        if (!Editor.Execute(command))
        {
            return false;
        }

        Selection.SelectOnly(command.NodeIds);
        FocusedNodeId = LastSelectedNode();
        StatusMessage = FlowStrings.Paste + " " + FlowStrings.Selected(command.NodeCount);
        OnSelectionChanged();
        return true;
    }

    public bool Undo()
    {
        if (Editor is null || !Editor.Undo())
        {
            return false;
        }

        Selection.Prune(Editor.Document);
        StatusMessage = FlowStrings.Undo + " " + (Editor.RedoLabel ?? string.Empty);
        OnSelectionChanged();
        return true;
    }

    public bool Redo()
    {
        if (Editor is null || !Editor.Redo())
        {
            return false;
        }

        Selection.Prune(Editor.Document);
        StatusMessage = FlowStrings.Redo + " " + (Editor.UndoLabel ?? string.Empty);
        OnSelectionChanged();
        return true;
    }

    public bool MoveSelection(double dx, double dy)
    {
        if (Document is null || Editor is null || Selection.NodeIds.Count == 0)
        {
            return false;
        }

        if (!Editor.CanEdit)
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return false;
        }

        MoveNodesCommand command = new(Document, Selection.NodeIds, dx, dy);
        if (!command.HasMovement || !Editor.Execute(command))
        {
            return false;
        }

        StatusMessage = command.Label;
        return true;
    }

    public bool ApplyAutoLayout()
    {
        if (Editor is null)
        {
            return false;
        }

        if (!Editor.CanEdit)
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return false;
        }

        AutoLayoutCommand command = new(Editor.Document);
        if (!Editor.Execute(command))
        {
            return false;
        }

        StatusMessage = FlowStrings.AutoLayout;
        InvalidateVisual();
        return true;
    }

    public FlowConnectionFeedback CheckConnection(string? fromId, string? toId)
    {
        if (Document is null)
        {
            return FlowConnectionFeedback.Refused(FlowStrings.KindMissing);
        }

        if (IsReadOnly)
        {
            return FlowConnectionFeedback.Refused(FlowStrings.RefusedReadOnly);
        }

        if (string.IsNullOrEmpty(fromId) || string.IsNullOrEmpty(toId))
        {
            return FlowConnectionFeedback.Refused("a connection needs both ends.");
        }

        if (string.Equals(fromId, toId, StringComparison.Ordinal))
        {
            return FlowConnectionFeedback.Refused("a node cannot connect to itself.");
        }

        if (Document.Node(fromId) is null || Document.Node(toId) is null)
        {
            return FlowConnectionFeedback.Refused("one of the nodes is gone.");
        }

        if (Document.Edges.Any(edge => string.Equals(edge.From, fromId, StringComparison.Ordinal)
            && string.Equals(edge.To, toId, StringComparison.Ordinal)))
        {
            return FlowConnectionFeedback.Refused("these two nodes are already connected.");
        }

        return FlowConnectionFeedback.Allowed;
    }

    public FlowConnectionFeedback BeginConnect(string fromId)
    {
        ArgumentNullException.ThrowIfNull(fromId);
        if (IsReadOnly)
        {
            StatusMessage = FlowStrings.RefusedReadOnly;
            return FlowConnectionFeedback.Refused(FlowStrings.RefusedReadOnly);
        }

        if (Document is null || Document.Node(fromId) is not { } source)
        {
            return FlowConnectionFeedback.Refused("the node is gone.");
        }

        ConnectFrom = fromId;
        ConnectCandidate = null;
        ConnectCandidate = NextCandidate(1);
        ConnectPointer = OutputPort(source);
        StatusMessage = FlowStrings.ConnectPrompt(fromId);
        InvalidateVisual();
        return FlowConnectionFeedback.Allowed;
    }

    public FlowConnectionFeedback ConnectionFeedback(string? toId) => CheckConnection(ConnectFrom, toId);

    public bool CompleteConnect(string? toId, out string? error)
    {
        error = null;
        FlowConnectionFeedback feedback = CheckConnection(ConnectFrom, toId);
        if (!feedback.Valid)
        {
            error = feedback.Reason;
            StatusMessage = FlowStrings.ConnectRefused(feedback.Reason ?? FlowStrings.KindMissing);
            InvalidateVisual();
            return false;
        }

        if (ConnectFrom is not { } from || toId is null)
        {
            error = "a connection needs both ends.";
            return false;
        }

        FlowEdge edge = new(from, toId);
        AddEdgeCommand command = new(edge);
        if (Editor is null || !Editor.Execute(command))
        {
            error = FlowStrings.RefusedReadOnly;
            return false;
        }

        Selection.Clear();
        Selection.ToggleEdge(edge.Key);
        StatusMessage = command.Label;
        ConnectFrom = null;
        ConnectCandidate = null;
        ConnectPointer = null;
        OnSelectionChanged();
        return true;
    }

    public void CancelConnect()
    {
        ConnectFrom = null;
        ConnectCandidate = null;
        ConnectPointer = null;
        StatusMessage = null;
        InvalidateVisual();
    }

    public string? NextCandidate(int delta)
    {
        if (Document is null || ConnectFrom is null)
        {
            return null;
        }

        List<string> candidates = [.. Document.Nodes
            .Where(node => !string.Equals(node.Id, ConnectFrom, StringComparison.Ordinal))
            .Select(node => node.Id)];
        if (candidates.Count == 0)
        {
            return null;
        }

        int current = ConnectCandidate is null ? -1 : candidates.IndexOf(ConnectCandidate);
        int next = ((current + delta) % candidates.Count + candidates.Count) % candidates.Count;
        ConnectCandidate = candidates[next];
        StatusMessage = FlowStrings.ConnectPrompt(ConnectFrom);
        InvalidateVisual();
        return ConnectCandidate;
    }

    public string MoveFocus(int delta)
    {
        FocusedPanelIndex = ((FocusedPanelIndex + delta) % PanelOrder.Count + PanelOrder.Count) % PanelOrder.Count;
        FocusRequested?.Invoke(this, new FlowFocusRequestedEventArgs(FocusedPanel));
        return FocusedPanel;
    }

    public string? MoveNodeFocus(int delta)
    {
        if (Document is null || Document.Nodes.Count == 0)
        {
            return null;
        }

        List<string> ids = [.. Document.Nodes.Select(node => node.Id)];
        int current = FocusedNodeId is null ? -1 : ids.IndexOf(FocusedNodeId);
        int next = ((current + delta) % ids.Count + ids.Count) % ids.Count;
        Select(ids[next]);
        return FocusedNodeId;
    }

    private string? LastSelectedNode() =>
        Selection.NodeIds.Count == 0 ? null : Selection.NodeIds[Selection.NodeIds.Count - 1];

    private static double Distance(Point a, Point b)
    {
        double dx = a.X - b.X;
        double dy = a.Y - b.Y;
        return Math.Sqrt((dx * dx) + (dy * dy));
    }

    private void OnSelectionChanged()
    {
        SelectionChanged?.Invoke(this, EventArgs.Empty);
        InvalidateVisual();
    }

    private void OnDocumentChanged()
    {
        Selection.Prune(Editor?.Document ?? throw new InvalidOperationException("No document."));
        DocumentChanged?.Invoke(this, EventArgs.Empty);
        InvalidateVisual();
    }

    private void OnStatusChanged() => StatusChanged?.Invoke(this, EventArgs.Empty);
}
