using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>
/// One slot for one context: the items the user may see, in render order, with their
/// expressions evaluated. The view is live — it re-evaluates whenever the context (the
/// draft) changes and raises <see cref="Changed"/> — and it owns the entry models, so a
/// busy action keeps its busy state across a refresh and a composite's button and the
/// toolbar's button are the same model.
/// </summary>
public sealed class SlotView : IDisposable
{
    private readonly ExtensionSet _set;
    private readonly ExtensionContext _context;
    private readonly ExtensionServices _services;
    private readonly string? _doctype;
    private readonly List<SlotEntry> _all = [];
    private readonly Dictionary<string, ActionModel> _actions = new(StringComparer.Ordinal);
    private readonly List<SlotEntry> _visible = [];
    private bool _disposed;

    internal SlotView(ExtensionSet set, string slot, ExtensionContext context, string? doctype, ExtensionServices services)
    {
        _set = set;
        Slot = slot;
        _context = context;
        _doctype = doctype;
        _services = services;
        foreach (IUiItem item in set.ItemsOf(slot, doctype))
        {
            _all.Add(Build(item));
        }
        _context.Changed += OnContextChanged;
        UpdateVisible();
    }

    /// <summary>The slot's full name.</summary>
    public string Slot { get; }

    /// <summary>The DocType the view was built for, when the slot is DocType-scoped.</summary>
    public string? Doctype => _doctype;

    /// <summary>The visible entries, in render order.</summary>
    public IReadOnlyList<SlotEntry> Entries => _visible;

    /// <summary>Every entry of the slot, visible or not, in render order.</summary>
    public IReadOnlyList<SlotEntry> AllEntries => _all;

    /// <summary>The document the view was built from.</summary>
    public ExtensionSet Set => _set;

    /// <summary>The dispatcher, shell services and query runner the entries use.</summary>
    public ExtensionServices Services => _services;

    /// <summary>Raised when the visible set or one entry's state changed.</summary>
    public event EventHandler? Changed;

    /// <summary>The live action for an id, sharing one busy state with a composite button.</summary>
    public ActionModel? ActionModelFor(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        if (_actions.TryGetValue(id, out ActionModel? existing))
        {
            return existing;
        }
        ActionSpec? spec = _set.Action(id);
        if (spec is null)
        {
            return null;
        }
        ActionModel model = new(spec, _context, _services, _set.Sink);
        _actions[id] = model;
        model.Changed += OnEntryChanged;
        return model;
    }

    /// <summary>
    /// Loads the slot's data: every panel's data blocks and every tile's prop queries.
    /// Cancellable and asynchronous; the renderer calls it when the slot is attached.
    /// </summary>
    public async Task LoadAsync(CancellationToken cancellationToken)
    {
        foreach (SlotEntry entry in _all)
        {
            switch (entry)
            {
                case PanelModel panel:
                    await panel.RefreshAsync(cancellationToken).ConfigureAwait(false);
                    break;
                case TileModel tile:
                    await tile.LoadAsync(cancellationToken).ConfigureAwait(false);
                    break;
                case TabModel tab:
                    await tab.Panel.RefreshAsync(cancellationToken).ConfigureAwait(false);
                    break;
                default:
                    break;
            }
        }
    }

    /// <summary>Re-evaluates every entry's visibility, enabled state and presentation expressions.</summary>
    public void Refresh()
    {
        foreach (SlotEntry entry in _all)
        {
            switch (entry)
            {
                case ActionModel action:
                    action.Refresh();
                    break;
                case PanelModel panel:
                    panel.Refresh();
                    break;
                case TileModel tile:
                    tile.Refresh();
                    break;
                case TabModel tab:
                    tab.Refresh();
                    break;
                default:
                    break;
            }
        }
        UpdateVisible();
    }

    /// <summary>Rebuilds the visible list without re-evaluating the entries (the models were
    /// just evaluated: building a view costs one evaluation per expression, never two).</summary>
    private void UpdateVisible()
    {
        List<SlotEntry> visible = [.. _all.Where(static entry => entry.IsVisible)];
        bool changed = visible.Count != _visible.Count || !visible.SequenceEqual(_visible);
        _visible.Clear();
        _visible.AddRange(visible);
        if (changed)
        {
            RaiseChanged();
        }
    }

    /// <summary>Reloads the panels after the document was saved.</summary>
    public async Task DocumentSavedAsync(CancellationToken cancellationToken)
    {
        foreach (SlotEntry entry in _all)
        {
            switch (entry)
            {
                case PanelModel panel:
                    await panel.DocumentSavedAsync(cancellationToken).ConfigureAwait(false);
                    break;
                case TabModel tab:
                    await tab.Panel.DocumentSavedAsync(cancellationToken).ConfigureAwait(false);
                    break;
                default:
                    break;
            }
        }
    }

    /// <summary>Unsubscribes from the context and the entries.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }
        _disposed = true;
        _context.Changed -= OnContextChanged;
        foreach (SlotEntry entry in _all)
        {
            entry.Changed -= OnEntryChanged;
        }
        foreach (ActionModel action in _actions.Values)
        {
            action.Changed -= OnEntryChanged;
        }
        _actions.Clear();
    }

    private SlotEntry Build(IUiItem item) => item switch
    {
        ActionSpec action => ActionModelFor(action.Id) ?? throw new InvalidOperationException("the item is an action of this set"),
        PanelSpec panel => Track(new PanelModel(panel, _context, _services, _set.Sink)),
        TileSpec tile => Track(new TileModel(tile, _context, _services, _set.Sink, this)),
        TabSpec tab => Track(new TabModel(tab, _context, _services, _set, _set.Sink)),
        _ => throw new InvalidOperationException("unknown slot item")
    };

    private SlotEntry Track(SlotEntry entry)
    {
        entry.Changed += OnEntryChanged;
        return entry;
    }

    private void OnEntryChanged(object? sender, EventArgs e) => RaiseChanged();

    private void OnContextChanged(object? sender, EventArgs e) => Refresh();

    private void RaiseChanged() => Changed?.Invoke(this, EventArgs.Empty);
}

/// <summary>A home tile: a composite whose props are literals or aggregate queries, with
/// the loading, empty and error states a remote aggregate needs.</summary>
public sealed class TileModel : SlotEntry
{
    private readonly TileSpec _spec;
    private readonly ExtensionContext _context;
    private readonly ExtensionServices _services;
    private readonly DiagnosticSink _sink;
    private readonly SlotView _owner;
    private ComponentModel? _component;
    private PanelDataState _state = PanelDataState.Idle;
    private string? _problem;

    internal TileModel(TileSpec spec, ExtensionContext context, ExtensionServices services, DiagnosticSink sink, SlotView owner)
        : base(spec.Id, spec.App, spec.Order)
    {
        _spec = spec;
        _context = context;
        _services = services;
        _sink = sink;
        _owner = owner;
        Refresh();
    }

    /// <summary>The tile's label.</summary>
    public string Label => _spec.Label;

    /// <summary>The tile's icon token, or null.</summary>
    public string? Icon => _spec.Icon;

    /// <summary>The expanded component, or null before the first load or after a failure.</summary>
    public ComponentModel? Component => _component;

    /// <summary>The expanded tree, or null.</summary>
    public ExpandedNode? Root => _component?.Root;

    /// <summary>The tile's data state.</summary>
    public PanelDataState State => _state;

    /// <summary>The failure's problem text, when the state is <see cref="PanelDataState.Error"/>.</summary>
    public string? Problem => _problem;

    /// <summary>True when a click opens the tile's destination.</summary>
    public bool OpensOnClick => _spec.Open is not null;

    /// <summary>Resolves the tile's props (a query prop runs its aggregate), then expands the component.</summary>
    public async Task LoadAsync(CancellationToken cancellationToken)
    {
        _state = PanelDataState.Loading;
        _problem = null;
        RaiseChanged();
        try
        {
            Dictionary<string, ExpressionValue> props = new(StringComparer.Ordinal);
            foreach (KeyValuePair<string, TileProp> prop in _spec.Props)
            {
                if (!prop.Value.IsQuery)
                {
                    props[prop.Key] = prop.Value.Literal ?? ExpressionValue.Null;
                    continue;
                }
                if (_services.Queries is null)
                {
                    throw new InvalidOperationException("the host wired no query runner");
                }
                QueryAggregate aggregate = await _services.Queries.RunAggregateAsync(prop.Value.Query!, cancellationToken).ConfigureAwait(false);
                props[prop.Key] = aggregate.Value;
            }
            ComponentSpec? component = _owner.Set.Component(_spec.Component);
            if (component is null)
            {
                _sink.Report(Id, ExtensionCodes.UnknownComponent, $"component {_spec.Component} is not in this document");
                _state = PanelDataState.Error;
                _problem = $"component {_spec.Component} is not in this document";
                RaiseChanged();
                return;
            }
            _component = ComponentModel.Expand(component, props, _context, _owner.Set, _owner.ActionModelFor);
            _state = _component.IsUsable ? PanelDataState.Ready : PanelDataState.Empty;
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            _state = PanelDataState.Idle;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            _state = PanelDataState.Error;
            _problem = error.Message;
            _sink.ReportOnce(Id, ExtensionCodes.QueryFailed, error.Message);
        }
        RaiseChanged();
    }

    /// <summary>Opens the tile's destination through the shell.</summary>
    public async Task OpenAsync(CancellationToken cancellationToken)
    {
        if (_spec.Open is not { } open || _services.Shell is null)
        {
            return;
        }
        IReadOnlyList<KeyValuePair<string, ExpressionValue>> filters =
            [.. open.Filters.Select(filter => new KeyValuePair<string, ExpressionValue>(filter.Key, filter.Value.Evaluate(_context, _sink, Id)))];
        await _services.Shell.OpenAsync(open.Destination, filters, cancellationToken).ConfigureAwait(false);
    }

    internal void Refresh()
    {
        bool visible = _context.HasPermission(_spec.Permission);
        if (visible && _spec.VisibleIf is not null)
        {
            visible = _spec.VisibleIf.EvaluateBool(_context, _sink, Id, fallback: false);
        }
        IsVisible = visible;
    }
}

/// <summary>A document tab: its label and the panel it shows.</summary>
public sealed class TabModel : SlotEntry
{
    private readonly TabSpec _spec;
    private readonly ExtensionContext _context;
    private readonly DiagnosticSink _sink;
    private readonly PanelModel? _panel;

    internal TabModel(TabSpec spec, ExtensionContext context, ExtensionServices services, ExtensionSet set, DiagnosticSink sink)
        : base(spec.Id, spec.App, spec.Order)
    {
        ArgumentNullException.ThrowIfNull(set);
        _spec = spec;
        _context = context;
        _sink = sink;
        PanelSpec? panel = set.Panel(spec.Panel);
        _panel = panel is null ? null : new PanelModel(panel, context, services, sink);
        if (_panel is null)
        {
            _sink.Report(Id, ExtensionCodes.UnknownPanel, $"panel {spec.Panel} is not in this document");
        }
        Refresh();
    }

    /// <summary>The tab's label.</summary>
    public string Label => _spec.Label;

    /// <summary>The tab's panel; only call it when <see cref="HasPanel"/> is true.</summary>
    public PanelModel Panel => _panel ?? throw new InvalidOperationException("the tab has no panel");

    /// <summary>True when the tab has a panel to show.</summary>
    public bool HasPanel => _panel is not null;

    internal void Refresh()
    {
        bool visible = _context.HasPermission(_spec.Permission) && _panel is not null;
        if (visible && _spec.VisibleIf is not null)
        {
            visible = _spec.VisibleIf.EvaluateBool(_context, _sink, Id, fallback: false);
        }
        IsVisible = visible;
        _panel?.Refresh();
    }
}
