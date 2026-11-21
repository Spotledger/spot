using Avalonia;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Input;

namespace Spotledger.Client.Graph;

/// <summary>Interaction half of <see cref="SpotLineageView"/>: click selects, double-click or
/// Enter opens the document through the navigation seam, a drag pans, the wheel zooms around
/// the pointer, and the keyboard pans, zooms, fits and walks the cards. Also the automation
/// peer: one invocable node peer per card, with a key-derived stable automation id.</summary>
public partial class SpotLineageView
{
    /// <summary>The time source the double-click window reads.</summary>
    public IGraphClock Clock { get; set; } = SystemGraphClock.Instance;

    private GraphNodeKey? _lastClickKey;
    private DateTimeOffset _lastClickStamp = DateTimeOffset.MinValue;

    /// <inheritdoc />
    protected override AutomationPeer OnCreateAutomationPeer() => new SpotLineageAutomationPeer(this);

    internal SpotLineageAutomationPeer? CachedPeer { get; private set; }

    internal AutomationPeer OnCreateAutomationPeerShim() => CachedPeer ??= new SpotLineageAutomationPeer(this);

    /// <inheritdoc />
    protected override void OnPointerPressed(PointerPressedEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        var point = e.GetCurrentPoint(this);
        if (!point.Properties.IsLeftButtonPressed)
        {
            base.OnPointerPressed(e);
            return;
        }

        Focus();
        var hit = HitTest(point.Position);
        if (hit is null)
        {
            _panning = true;
            _panStart = point.Position;
            _panOriginX = PanX;
            _panOriginY = PanY;
            e.Pointer.Capture(this);
            e.Handled = true;
            return;
        }

        var now = Clock.Now;
        var doubleClick = _lastClickKey == hit.Key && (now - _lastClickStamp).TotalMilliseconds < 500;
        _lastClickKey = hit.Key;
        _lastClickStamp = now;
        Select(hit.Key);
        if (doubleClick)
        {
            Activate(hit);
        }

        e.Handled = true;
    }

    /// <inheritdoc />
    protected override void OnPointerMoved(PointerEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        if (!_panning)
        {
            base.OnPointerMoved(e);
            return;
        }

        var point = e.GetCurrentPoint(this);
        if (!point.Properties.IsLeftButtonPressed)
        {
            _panning = false;
            return;
        }

        PanX = _panOriginX + (point.Position.X - _panStart.X);
        PanY = _panOriginY + (point.Position.Y - _panStart.Y);
        InvalidateVisual();
        e.Handled = true;
    }

    /// <inheritdoc />
    protected override void OnPointerReleased(PointerReleasedEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        _panning = false;
        e.Pointer.Capture(null);
        base.OnPointerReleased(e);
    }

    /// <inheritdoc />
    protected override void OnPointerWheelChanged(PointerWheelEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        var factor = e.Delta.Y > 0 ? 1.1 : 1 / 1.1;
        SetZoom(Zoom * factor, e.GetPosition(this));
        e.Handled = true;
    }

    /// <inheritdoc />
    protected override void OnKeyDown(KeyEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        var control = e.KeyModifiers.HasFlag(KeyModifiers.Control) || e.KeyModifiers.HasFlag(KeyModifiers.Meta);
        switch (e.Key)
        {
            case Key.Left:
                MoveSelection(-1);
                break;
            case Key.Right:
                MoveSelection(1);
                break;
            case Key.Up:
                PanBy(0, 40);
                break;
            case Key.Down:
                PanBy(0, -40);
                break;
            case Key.Home:
                MoveSelection(-int.MaxValue / 2);
                break;
            case Key.End:
                MoveSelection(int.MaxValue / 2);
                break;
            case Key.Enter:
                if (SelectedKey is { } key && Layout?.Find(key) is { } placement)
                {
                    Activate(placement);
                }

                break;
            case Key.OemPlus:
            case Key.Add:
                SetZoom(Zoom * 1.1);
                break;
            case Key.OemMinus:
            case Key.Subtract:
                SetZoom(Zoom / 1.1);
                break;
            case Key.D0 when control:
                ZoomToFit();
                break;
            default:
                base.OnKeyDown(e);
                return;
        }

        e.Handled = true;
    }

    internal IReadOnlyList<LineageNodePlacement> PeerNodes()
    {
        if (Layout is not { } layout)
        {
            return [];
        }

        const int peerLimit = 500;
        if (layout.Nodes.Count <= peerLimit)
        {
            return layout.Nodes;
        }

        var buffer = new List<LineageNodePlacement>(peerLimit);
        for (var i = 0; i < peerLimit; i++)
        {
            buffer.Add(layout.Nodes[i]);
        }

        return buffer;
    }
}

/// <summary>The view peer: a pane whose children are the cards.</summary>
public sealed class SpotLineageAutomationPeer : ControlAutomationPeer
{
    private readonly SpotLineageView _owner;

    /// <summary>Creates the peer.</summary>
    /// <param name="owner">The view.</param>
    public SpotLineageAutomationPeer(SpotLineageView owner) : base(owner) => _owner = owner;

    /// <inheritdoc />
    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Pane;

    /// <inheritdoc />
    protected override string GetClassNameCore() => nameof(SpotLineageView);

    /// <inheritdoc />
    protected override string? GetNameCore() => "Spotledger lineage view";

    /// <inheritdoc />
    protected override string? GetAutomationIdCore() => "graph-lineage";

    /// <inheritdoc />
    protected override bool IsControlElementCore() => true;

    /// <inheritdoc />
    protected override bool IsContentElementCore() => true;

    /// <inheritdoc />
    protected override IReadOnlyList<AutomationPeer>? GetChildrenCore()
    {
        var nodes = _owner.PeerNodes();
        var peers = new List<AutomationPeer>(nodes.Count);
        foreach (var placement in nodes)
        {
            peers.Add(new SpotLineageNodeAutomationPeer(_owner, placement));
        }

        return peers;
    }
}

/// <summary>Peer for one lineage card: its title, status and amount in the name (the locked
/// placeholder when redacted), a key-derived automation id, and the invoke pattern that opens
/// the document exactly like a double-click.</summary>
public sealed class SpotLineageNodeAutomationPeer : ControlAutomationPeer, IInvokeProvider
{
    private readonly SpotLineageView _owner;
    private readonly LineageNodePlacement _placement;

    /// <summary>Creates the card peer.</summary>
    /// <param name="owner">The view.</param>
    /// <param name="placement">The card.</param>
    public SpotLineageNodeAutomationPeer(SpotLineageView owner, LineageNodePlacement placement) : base(owner)
    {
        _owner = owner;
        _placement = placement;
    }

    /// <inheritdoc />
    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.ListItem;

    /// <inheritdoc />
    protected override string GetClassNameCore() => nameof(LineageNode);

    /// <inheritdoc />
    protected override string? GetNameCore()
    {
        var node = _placement.Node;
        return node.IsLocked
            ? LineageNode.LockedTitle
            : $"{node.Title}, {node.Status}, {node.Amount}";
    }

    /// <inheritdoc />
    protected override string? GetAutomationIdCore()
        => string.Concat("graph-lineage-node-", _placement.Key.ToString().Replace('/', '-').Replace('.', '-'));

    /// <inheritdoc />
    protected override bool IsControlElementCore() => true;

    /// <inheritdoc />
    protected override bool IsContentElementCore() => true;

    /// <inheritdoc />
    public void Invoke() => _owner.Activate(_placement);
}
