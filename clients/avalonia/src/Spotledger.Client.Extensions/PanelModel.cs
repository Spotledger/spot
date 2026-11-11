using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>One label and one display text of a read-only block.</summary>
/// <param name="Label">The caption.</param>
/// <param name="Text">The display text (exact for a decimal).</param>
public sealed record BlockEntry(string Label, string Text);

/// <summary>One block of a panel, live: its state, its data and the retry a failure offers.</summary>
public abstract class PanelBlockView
{
    /// <summary>Creates the view of one block.</summary>
    protected PanelBlockView(PanelModel owner, PanelBlock block)
    {
        ArgumentNullException.ThrowIfNull(owner);
        ArgumentNullException.ThrowIfNull(block);
        Owner = owner;
        Block = block;
    }

    /// <summary>The panel this block belongs to.</summary>
    public PanelModel Owner { get; }

    /// <summary>The parsed block.</summary>
    public PanelBlock Block { get; }

    /// <summary>Raised when the block's data or state changed.</summary>
    public event EventHandler? Changed;

    /// <summary>The block's data state; a presentation block is always <see cref="PanelDataState.Ready"/>.</summary>
    public virtual PanelDataState State => PanelDataState.Ready;

    /// <summary>Re-evaluates the block's expressions (or does nothing for a data block).</summary>
    internal virtual void Refresh()
    {
    }

    /// <summary>Loads the block's data; a presentation block does nothing.</summary>
    public virtual Task LoadAsync(CancellationToken cancellationToken) => Task.CompletedTask;

    /// <summary>Retries a failed load.</summary>
    public virtual Task RetryAsync(CancellationToken cancellationToken) => LoadAsync(cancellationToken);

    /// <summary>Raises <see cref="Changed" /> for a subclass.</summary>
    protected void RaiseChanged() => Changed?.Invoke(this, EventArgs.Empty);
}

/// <summary><c>field_list</c>: named document fields, read-only, as label and exact text.</summary>
public sealed class FieldListBlockView : PanelBlockView
{
    private readonly ExtensionContext _context;
    private readonly DiagnosticSink _sink;
    private List<BlockEntry> _items = [];

    internal FieldListBlockView(PanelModel owner, FieldListBlock block, ExtensionContext context, DiagnosticSink sink)
        : base(owner, block)
    {
        _context = context;
        _sink = sink;
        Refresh();
    }

    /// <summary>The fields, in the block's order.</summary>
    public IReadOnlyList<BlockEntry> Items => _items;

    internal override void Refresh()
    {
        FieldListBlock block = (FieldListBlock)Block;
        List<BlockEntry> items = new(block.Fields.Count);
        foreach (string field in block.Fields)
        {
            string label = _context.Draft?.Model.Field(field)?.Label ?? field;
            string text = _context.Draft?.Get(field) is { } value ? DisplayText.For(value) : string.Empty;
            items.Add(new BlockEntry(label, text));
        }
        _items = items;
        RaiseChanged();
        _ = _sink;
    }
}

/// <summary><c>key_value</c>: one expression per row, evaluated against the current document.</summary>
public sealed class KeyValueBlockView : PanelBlockView
{
    private readonly ExtensionContext _context;
    private readonly DiagnosticSink _sink;
    private readonly string _ownerId;
    private List<BlockEntry> _items = [];

    internal KeyValueBlockView(PanelModel owner, KeyValueBlock block, ExtensionContext context, DiagnosticSink sink)
        : base(owner, block)
    {
        _context = context;
        _sink = sink;
        _ownerId = owner.Id;
        Refresh();
    }

    /// <summary>The evaluated rows.</summary>
    public IReadOnlyList<BlockEntry> Items => _items;

    internal override void Refresh()
    {
        KeyValueBlock block = (KeyValueBlock)Block;
        _items = [.. block.Items.Select(item =>
            new BlockEntry(item.Label, DisplayText.For(item.Value.Evaluate(_context, _sink, _ownerId))))];
        RaiseChanged();
    }
}

/// <summary><c>text</c>: the restricted markdown, already parsed to an inline model.</summary>
public sealed class TextBlockView : PanelBlockView
{
    internal TextBlockView(PanelModel owner, TextPanelBlock block) : base(owner, block)
    {
    }

    /// <summary>The parsed markdown the renderer draws.</summary>
    public MarkdownModel Model => ((TextPanelBlock)Block).Model;
}

/// <summary><c>list_of_links</c>: registered destinations the user may open.</summary>
public sealed class ListOfLinksBlockView : PanelBlockView
{
    private readonly ExtensionContext _context;
    private readonly IShellServices? _shell;
    private readonly DiagnosticSink _sink;
    private readonly string _ownerId;

    internal ListOfLinksBlockView(PanelModel owner, ListOfLinksBlock block, ExtensionContext context, IShellServices? shell, DiagnosticSink sink)
        : base(owner, block)
    {
        _context = context;
        _shell = shell;
        _sink = sink;
        _ownerId = owner.Id;
    }

    /// <summary>The links, in order.</summary>
    public IReadOnlyList<LinkItem> Items => ((ListOfLinksBlock)Block).Items;

    /// <summary>Opens one link's destination through the shell; never an external URL.</summary>
    public async Task OpenAsync(LinkItem link, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(link);
        if (_shell is null)
        {
            _sink.Report(_ownerId, ExtensionCodes.DispatchFailed, "the host wired no shell services");
            return;
        }
        IReadOnlyList<KeyValuePair<string, ExpressionValue>> filters =
            [.. link.Open.Filters.Select(filter => new KeyValuePair<string, ExpressionValue>(filter.Key, filter.Value.Evaluate(_context, _sink, _ownerId)))];
        await _shell.OpenAsync(link.Open.Destination, filters, cancellationToken).ConfigureAwait(false);
    }
}

/// <summary><c>empty_state</c>: the text a panel shows when there is nothing to show.</summary>
public sealed class EmptyStateBlockView : PanelBlockView
{
    internal EmptyStateBlockView(PanelModel owner, EmptyStateBlock block) : base(owner, block)
    {
    }

    /// <summary>The text.</summary>
    public string Text => ((EmptyStateBlock)Block).Text;

    /// <summary>The icon token, or null.</summary>
    public string? Icon => ((EmptyStateBlock)Block).Icon;
}

/// <summary><c>table</c>: rows from a declared server query, paged and capped at the
/// block's <c>max_rows</c>.</summary>
public sealed class TableBlockView : PanelBlockView
{
    private readonly QueryRunnerAccess _queries;
    private readonly DiagnosticSink _sink;
    private readonly string _ownerId;
    private List<IReadOnlyList<string>> _rows = [];
    private PanelDataState _state = PanelDataState.Idle;
    private string? _problem;
    private bool _hasMore;

    internal TableBlockView(PanelModel owner, TableBlock block, QueryRunnerAccess queries, DiagnosticSink sink)
        : base(owner, block)
    {
        _queries = queries;
        _sink = sink;
        _ownerId = owner.Id;
    }

    /// <summary>The query the block reads.</summary>
    public string Query => ((TableBlock)Block).Query;

    /// <summary>The declared maximum number of rows; the block never holds more.</summary>
    public int MaxRows => ((TableBlock)Block).MaxRows;

    /// <summary>True when a row links to its document.</summary>
    public bool OpensRows => ((TableBlock)Block).Open;

    /// <summary>The column captions, in order.</summary>
    public IReadOnlyList<string> ColumnLabels =>
        [.. ((TableBlock)Block).Columns.Select(column => column.Label ?? column.Field)];

    /// <summary>The column field names, in order.</summary>
    public IReadOnlyList<string> ColumnFields => [.. ((TableBlock)Block).Columns.Select(column => column.Field)];

    /// <summary>The rows held, each with one cell per column.</summary>
    public IReadOnlyList<IReadOnlyList<string>> Rows => _rows;

    /// <summary>The block's data state.</summary>
    public override PanelDataState State => _state;

    /// <summary>The failure's problem text, when the state is <see cref="PanelDataState.Error"/>.</summary>
    public string? Problem => _problem;

    /// <summary>True when the query has more rows than the block holds (it never holds more than <see cref="MaxRows"/>).</summary>
    public bool HasMore => _hasMore;

    /// <summary>Loads the first page, replacing whatever the block held.</summary>
    public override async Task LoadAsync(CancellationToken cancellationToken)
    {
        _state = PanelDataState.Loading;
        _problem = null;
        RaiseChanged();
        try
        {
            QueryRows page = await _queries.Run(Query, new QueryPage(0, MaxRows), cancellationToken).ConfigureAwait(false);
            _rows = Project(page.Rows);
            _hasMore = page.HasMore && _rows.Count < MaxRows;
            _state = _rows.Count == 0 ? PanelDataState.Empty : PanelDataState.Ready;
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            _state = _rows.Count == 0 ? PanelDataState.Idle : PanelDataState.Ready;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            Fail(error);
        }
        RaiseChanged();
    }

    /// <summary>Loads the next page when the server has more and the block has room.</summary>
    public async Task LoadMoreAsync(CancellationToken cancellationToken)
    {
        if (!_hasMore || _rows.Count >= MaxRows || _state is PanelDataState.Loading)
        {
            return;
        }
        _state = PanelDataState.Loading;
        RaiseChanged();
        try
        {
            QueryRows page = await _queries.Run(Query, new QueryPage(_rows.Count, MaxRows - _rows.Count), cancellationToken).ConfigureAwait(false);
            List<IReadOnlyList<string>> combined = [.. _rows, .. Project(page.Rows)];
            _rows = combined.Count > MaxRows ? [.. combined.Take(MaxRows)] : combined;
            _hasMore = page.HasMore && _rows.Count < MaxRows;
            _state = _rows.Count == 0 ? PanelDataState.Empty : PanelDataState.Ready;
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            _state = PanelDataState.Ready;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            Fail(error);
        }
        RaiseChanged();
    }

    /// <summary>The first cell of a row, which a rows query returns as the document id.</summary>
    public string? RowDocumentId(int index) =>
        index >= 0 && index < _rows.Count && _rows[index].Count > 0 ? _rows[index][0] : null;

    /// <summary>Asks the shell to open the document one row stands for. The shell resolves
    /// the DocType from its destination registry; the client never guesses one.</summary>
    public async Task OpenRowAsync(int index, CancellationToken cancellationToken)
    {
        if (!OpensRows || _queries.Shell is not { } shell)
        {
            return;
        }
        string? id = RowDocumentId(index);
        if (string.IsNullOrEmpty(id))
        {
            return;
        }
        await shell.OpenDocumentAsync(id, cancellationToken).ConfigureAwait(false);
    }

    private void Fail(Exception error)
    {
        _state = PanelDataState.Error;
        _problem = error.Message;
        _sink.ReportOnce(_ownerId, ExtensionCodes.QueryFailed, $"{Query}: {error.Message}");
    }

    private List<IReadOnlyList<string>> Project(IReadOnlyList<IReadOnlyList<string>> rows)
    {
        int columns = ColumnFields.Count;
        List<IReadOnlyList<string>> projected = new(rows.Count);
        foreach (IReadOnlyList<string> row in rows)
        {
            string[] cells = new string[columns];
            for (int index = 0; index < columns; index++)
            {
                cells[index] = index < row.Count ? row[index] ?? string.Empty : string.Empty;
            }
            projected.Add(cells);
        }
        return projected;
    }
}

/// <summary><c>stat</c>: one aggregate from a declared query, formatted exactly.</summary>
public sealed class StatBlockView : PanelBlockView
{
    private readonly ExtensionContext _context;
    private readonly QueryRunnerAccess _queries;
    private readonly DiagnosticSink _sink;
    private readonly string _ownerId;
    private PanelDataState _state = PanelDataState.Idle;
    private string? _problem;
    private string _text = string.Empty;

    internal StatBlockView(PanelModel owner, StatBlock block, ExtensionContext context, QueryRunnerAccess queries, DiagnosticSink sink)
        : base(owner, block)
    {
        _context = context;
        _queries = queries;
        _sink = sink;
        _ownerId = owner.Id;
    }

    /// <summary>The caption.</summary>
    public string Label => ((StatBlock)Block).Label;

    /// <summary>The declared format.</summary>
    public string Format => ((StatBlock)Block).Format;

    /// <summary>The query the block reads.</summary>
    public string Query => ((StatBlock)Block).Query;

    /// <summary>The formatted aggregate.</summary>
    public string Text => _text;

    /// <summary>The block's data state.</summary>
    public override PanelDataState State => _state;

    /// <summary>The failure's problem text.</summary>
    public string? Problem => _problem;

    /// <summary>Loads the aggregate.</summary>
    public override async Task LoadAsync(CancellationToken cancellationToken)
    {
        _state = PanelDataState.Loading;
        _problem = null;
        RaiseChanged();
        try
        {
            QueryAggregate aggregate = await _queries.RunAggregate(Query, cancellationToken).ConfigureAwait(false);
            _text = ExtensionFormatting.Stat(aggregate.Value, Format, _context.CurrencySymbol);
            _state = PanelDataState.Ready;
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            _state = PanelDataState.Idle;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            _state = PanelDataState.Error;
            _problem = error.Message;
            _sink.ReportOnce(_ownerId, ExtensionCodes.QueryFailed, $"{Query}: {error.Message}");
        }
        RaiseChanged();
    }
}

/// <summary>A panel: its label, its blocks, and the refresh that reloads every data block.
/// A panel is rebuilt per slot view and re-evaluated when the draft changes; the shell
/// calls <see cref="DocumentSavedAsync"/> after a save so a stat or a table shows the
/// saved data.</summary>
public sealed class PanelModel : SlotEntry
{
    private readonly PanelSpec _spec;
    private readonly ExtensionContext _context;
    private readonly DiagnosticSink _sink;

    internal PanelModel(PanelSpec spec, ExtensionContext context, ExtensionServices services, DiagnosticSink sink)
        : base(spec.Id, spec.App, spec.Order)
    {
        _spec = spec;
        _context = context;
        _sink = sink;
        QueryRunnerAccess access = new(services.Queries, services.Shell);
        List<PanelBlockView> blocks = [];
        foreach (PanelBlock block in spec.Blocks)
        {
            blocks.Add(block switch
            {
                FieldListBlock fieldList => new FieldListBlockView(this, fieldList, context, sink),
                KeyValueBlock keyValue => new KeyValueBlockView(this, keyValue, context, sink),
                TextPanelBlock text => new TextBlockView(this, text),
                TableBlock table => new TableBlockView(this, table, access, sink),
                StatBlock stat => new StatBlockView(this, stat, context, access, sink),
                ListOfLinksBlock links => new ListOfLinksBlockView(this, links, context, services.Shell, sink),
                EmptyStateBlock empty => new EmptyStateBlockView(this, empty),
                _ => throw new InvalidOperationException("unknown panel block")
            });
        }
        Blocks = blocks;
        foreach (PanelBlockView block in Blocks)
        {
            block.Changed += OnBlockChanged;
        }
        Refresh();
    }

    /// <summary>The panel's label.</summary>
    public string Label => _spec.Label;

    /// <summary>The panel's blocks, in order.</summary>
    public IReadOnlyList<PanelBlockView> Blocks { get; }

    /// <summary>The panel's specification.</summary>
    public PanelSpec Spec => _spec;

    /// <summary>True while any data block is loading.</summary>
    public bool IsLoading => Blocks.Any(block => block.State == PanelDataState.Loading);

    /// <summary>The text the panel shows when a data block has nothing: its own
    /// <c>empty_state</c> block, or a neutral default.</summary>
    public string EmptyText => Blocks.OfType<EmptyStateBlockView>().Select(block => block.Text).FirstOrDefault() ?? "Nothing to show";

    /// <summary>Reloads every data block. Cancellable; never blocks the caller's thread.</summary>
    public async Task RefreshAsync(CancellationToken cancellationToken)
    {
        foreach (PanelBlockView block in Blocks)
        {
            await block.LoadAsync(cancellationToken).ConfigureAwait(false);
        }
    }

    /// <summary>Reloads the panel after the document was saved.</summary>
    public Task DocumentSavedAsync(CancellationToken cancellationToken) => RefreshAsync(cancellationToken);

    /// <summary>Re-evaluates visibility and the presentation blocks after a draft change.</summary>
    internal void Refresh()
    {
        bool visible = _context.HasPermission(_spec.Permission);
        if (visible && _spec.VisibleIf is not null)
        {
            visible = _spec.VisibleIf.EvaluateBool(_context, _sink, Id, fallback: false);
        }
        IsVisible = visible;
        foreach (PanelBlockView block in Blocks)
        {
            block.Refresh();
        }
    }

    private void OnBlockChanged(object? sender, EventArgs e) => RaiseChanged();
}

/// <summary>The query runner and shell one panel's data blocks share, with the "no runner"
/// case folded into one place so every block reports the same diagnostic.</summary>
internal sealed class QueryRunnerAccess(IQueryRunner? queries, IShellServices? shell)
{
    internal IShellServices? Shell { get; } = shell;

    internal Task<QueryRows> Run(string query, QueryPage page, CancellationToken cancellationToken) =>
        queries is null
            ? throw new InvalidOperationException("the host wired no query runner")
            : queries.RunRowsAsync(query, page, cancellationToken);

    internal Task<QueryAggregate> RunAggregate(string query, CancellationToken cancellationToken) =>
        queries is null
            ? throw new InvalidOperationException("the host wired no query runner")
            : queries.RunAggregateAsync(query, cancellationToken);
}
