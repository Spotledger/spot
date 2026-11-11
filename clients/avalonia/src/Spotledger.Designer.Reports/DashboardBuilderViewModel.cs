using System.Collections.ObjectModel;

namespace Spotledger.Designer.Reports;

/// <summary>A kind of tile in the gallery, with the size it starts at.</summary>
public sealed record TileTemplate(TileKind Kind, string Label, string Description, int Width, int Height);

/// <summary>
/// The dashboard builder: a gallery of tile kinds, a layout grid (drag a tile onto it, or move it
/// with the keyboard), per-tile settings, filters shared across tiles, and a bounded preview.
/// </summary>
/// <remarks>
/// The first free place for a new tile is only a suggestion; the server checks every place and
/// refuses a tile that does not fit or overlaps another, and this view-model shows that refusal.
/// </remarks>
public sealed class DashboardBuilderViewModel : ObservableObject
{
    private readonly IBuilderServer _server;
    private readonly string _draftId;
    private readonly int _previewRows;
    private DashboardDraft? _draft;
    private long _version;
    private string? _selectedTileId;
    private string _status = string.Empty;
    private string _publishMessage = string.Empty;
    private ReportSummary? _selectedReport;
    private string _tileTitleEdit = string.Empty;
    private bool _tileUsesFiltersEdit = true;
    private Place? _dropTarget;
    private bool _dropTargetIsFree;

    /// <summary>Creates the builder over a server draft.</summary>
    public DashboardBuilderViewModel(IBuilderServer server, string draftId, int previewRows = ReportBuilderViewModel.DefaultPreviewRows)
    {
        _server = server ?? throw new ArgumentNullException(nameof(server));
        _draftId = draftId ?? throw new ArgumentNullException(nameof(draftId));
        _previewRows = previewRows;
        PublishCommand = new AsyncRelayCommand(_ => PublishAsync(), _ => CanPublish);
        RefreshCommand = new AsyncRelayCommand(_ => RefreshAsync());
        AddTileCommand = new AsyncRelayCommand(template => template is TileTemplate t ? AddFromGalleryAsync(t) : Task.CompletedTask, _ => Draft is not null);
        RemoveTileCommand = new AsyncRelayCommand(_ => RemoveSelectedAsync(), _ => SelectedTile is not null);
        ApplyTileSettingsCommand = new AsyncRelayCommand(_ => SetTileSettingsAsync(TileTitleEdit, TileUsesFiltersEdit), _ => SelectedTile is not null);

        // A failure that escapes a command is shown in the status line, never swallowed.
        foreach (var command in new[] { PublishCommand, RefreshCommand, AddTileCommand, RemoveTileCommand, ApplyTileSettingsCommand })
        {
            command.ExecutionFailed += exception => Status = Guard.Describe(exception);
        }
    }

    /// <summary>Adds a tile of the kind given as the command parameter (a <see cref="TileTemplate"/>) for <see cref="SelectedReport"/>.</summary>
    public AsyncRelayCommand AddTileCommand { get; }

    /// <summary>Removes the selected tile.</summary>
    public AsyncRelayCommand RemoveTileCommand { get; }

    /// <summary>Applies <see cref="TileTitleEdit"/> and <see cref="TileUsesFiltersEdit"/> to the selected tile.</summary>
    public AsyncRelayCommand ApplyTileSettingsCommand { get; }

    /// <summary>The report the next gallery tile draws on.</summary>
    public ReportSummary? SelectedReport
    {
        get => _selectedReport;
        set => SetField(ref _selectedReport, value);
    }

    /// <summary>The title being edited for the selected tile.</summary>
    public string TileTitleEdit
    {
        get => _tileTitleEdit;
        set => SetField(ref _tileTitleEdit, value ?? string.Empty);
    }

    /// <summary>Whether the selected tile follows the shared filters, as being edited.</summary>
    public bool TileUsesFiltersEdit
    {
        get => _tileUsesFiltersEdit;
        set => SetField(ref _tileUsesFiltersEdit, value);
    }

    /// <summary>One input per shared filter, whose values the preview uses.</summary>
    public ObservableCollection<ParameterInput> FilterInputs { get; } = new();

    /// <summary>Runs a view's action; a failed server call is shown in the status line instead of escaping.</summary>
    public Task<bool> TryAsync(Func<Task<bool>> work) => Guard.RunAsync(work, message => Status = message);

    /// <summary>Shows a problem the view found before anything was sent.</summary>
    public void ReportProblem(string message) => Status = message;

    /// <summary>Adds a tile of a gallery kind for <see cref="SelectedReport"/> at the first free place.</summary>
    public Task<bool> AddFromGalleryAsync(TileTemplate template) => AddFromGalleryCoreAsync(template, null);

    /// <summary>
    /// Adds a tile of a gallery kind where it was dropped: the same op as
    /// <see cref="AddFromGalleryAsync"/>, with the place the person pointed at instead of the first
    /// free one. The server checks that place as it checks every one, and a refusal is shown here.
    /// </summary>
    /// <param name="template">The gallery kind that was dragged.</param>
    /// <param name="place">The cell the tile was dropped on (already snapped to the grid).</param>
    public Task<bool> AddFromGalleryAtAsync(TileTemplate template, Place place)
    {
        ArgumentNullException.ThrowIfNull(place);
        return AddFromGalleryCoreAsync(template, place);
    }

    private async Task<bool> AddFromGalleryCoreAsync(TileTemplate template, Place? at)
    {
        ArgumentNullException.ThrowIfNull(template);
        if (SelectedReport is not { } report)
        {
            Status = "Choose a report for the tile first.";
            return false;
        }

        IReadOnlyList<string>? columns = null;
        ChartSpec? chart = null;
        switch (template.Kind)
        {
            case TileKind.Number:
                if (report.Columns.Count == 0)
                {
                    Status = "That report has no column to show as a number.";
                    return false;
                }

                columns = new[] { report.Columns[0] };
                break;
            case TileKind.List:
                columns = report.Columns.Take(3).ToList();
                break;
            case TileKind.Chart:
                if (report.Columns.Count < 2)
                {
                    Status = "A chart needs a report with at least two columns.";
                    return false;
                }

                chart = new ChartSpec(ChartKind.Bar, report.Columns[0], new[] { report.Columns[1] });
                break;
        }

        return await AddTileAsync(template.Kind, report.Name, report.Label, columns, chart, at).ConfigureAwait(true);
    }

    /// <summary>The kinds of tile that can be added.</summary>
    public static IReadOnlyList<TileTemplate> Gallery { get; } = new[]
    {
        new TileTemplate(TileKind.Number, "Number", "One large figure from a report column.", 3, 2),
        new TileTemplate(TileKind.Chart, "Chart", "A bar, line or pie chart over a report.", 6, 4),
        new TileTemplate(TileKind.Table, "Table", "The whole report as a table.", 12, 4),
        new TileTemplate(TileKind.List, "List", "A short list of chosen columns.", 4, 4),
    };

    /// <summary>Publishes the dashboard.</summary>
    public AsyncRelayCommand PublishCommand { get; }

    /// <summary>Validates again and refreshes the preview.</summary>
    public AsyncRelayCommand RefreshCommand { get; }

    /// <summary>The draft as the server last returned it.</summary>
    public DashboardDraft? Draft
    {
        get => _draft;
        private set
        {
            if (SetField(ref _draft, value))
            {
                SyncFilterInputs();
                SyncTileSettings();
                AddTileCommand.RaiseCanExecuteChanged();
                RemoveTileCommand.RaiseCanExecuteChanged();
                ApplyTileSettingsCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>The published reports a tile can draw on.</summary>
    public ObservableCollection<ReportSummary> Reports { get; } = new();

    /// <summary>The server's validation issues in plain language.</summary>
    public ObservableCollection<BuilderIssue> Issues { get; } = new();

    /// <summary>The bounded preview of every tile.</summary>
    public ObservableCollection<TilePreview> TilePreviews { get; } = new();

    /// <summary>The values of the shared filters used for the preview.</summary>
    public Dictionary<string, FilterValue> FilterValues { get; } = new();

    /// <summary>The selected tile's id, for the keyboard commands and the settings panel.</summary>
    public string? SelectedTileId
    {
        get => _selectedTileId;
        set
        {
            if (SetField(ref _selectedTileId, value))
            {
                OnPropertyChanged(nameof(SelectedTile));
                SyncTileSettings();
                RemoveTileCommand.RaiseCanExecuteChanged();
                ApplyTileSettingsCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>The selected tile.</summary>
    public Tile? SelectedTile => Draft?.Tiles.FirstOrDefault(t => t.Id == _selectedTileId);

    /// <summary>The grid width in cells, or zero before the draft is open.</summary>
    public int GridColumns => Draft?.GridColumns ?? 0;

    /// <summary>The grid height in cells, or zero before the draft is open.</summary>
    public int GridRows => Draft?.GridRows ?? 0;

    /// <summary>The last thing that happened, or the last refusal, in plain words.</summary>
    public string Status
    {
        get => _status;
        private set => SetField(ref _status, value);
    }

    /// <summary>What the last publish said.</summary>
    public string PublishMessage
    {
        get => _publishMessage;
        private set => SetField(ref _publishMessage, value);
    }

    /// <summary>Whether validation reports no blocking issue and there is a tile to publish.</summary>
    public bool CanPublish => Draft is { Tiles.Count: > 0 } && Issues.All(i => !i.IsError);

    /// <summary>Opens the draft and the list of reports.</summary>
    public async Task OpenAsync(CancellationToken cancellationToken = default)
    {
        await Guard.RunAsync(async () =>
        {
            var outcome = await _server.OpenDashboardAsync(_draftId, cancellationToken).ConfigureAwait(true);
            if (!outcome.Ok)
            {
                Status = outcome.Refusal?.Message ?? "The dashboard could not be opened.";
                return false;
            }

            Take(outcome);
            Reports.Clear();
            foreach (var report in await _server.ListReportsAsync(cancellationToken).ConfigureAwait(true))
            {
                Reports.Add(report);
            }

            SelectedReport ??= Reports.FirstOrDefault();
            await RefreshAsync(cancellationToken).ConfigureAwait(true);
            return true;
        }, message => Status = message).ConfigureAwait(true);
    }

    /// <summary>
    /// Adds a tile of a gallery kind drawing on a report. A drag onto the grid passes the cell it
    /// was dropped on; the keyboard path leaves <paramref name="at"/> empty and gets the first free place.
    /// </summary>
    public async Task<bool> AddTileAsync(TileKind kind, string report, string title, IReadOnlyList<string>? columns = null, ChartSpec? chart = null, Place? at = null)
    {
        if (Draft is null)
        {
            return false;
        }

        var template = Gallery.First(t => t.Kind == kind);
        var place = at ?? FirstFreePlace(template.Width, template.Height);
        if (place is null)
        {
            Status = "The dashboard is full. Make room or remove a tile first.";
            return false;
        }

        var id = NextTileId(kind);
        var tile = new Tile(id, title, kind, report, place, columns ?? Array.Empty<string>(), chart);
        if (!await ApplyAsync(new DashboardOp.AddTile(tile)).ConfigureAwait(true))
        {
            return false;
        }

        SelectedTileId = id;
        return true;
    }

    /// <summary>Removes the selected tile.</summary>
    public async Task<bool> RemoveSelectedAsync()
    {
        if (SelectedTile is not { } tile)
        {
            return false;
        }

        var removed = await ApplyAsync(new DashboardOp.RemoveTile(tile.Id)).ConfigureAwait(true);
        if (removed)
        {
            SelectedTileId = null;
        }

        return removed;
    }

    /// <summary>Drops a tile at a place (the drag path).</summary>
    public Task<bool> MoveTileAsync(string id, Place place) => ApplyAsync(new DashboardOp.MoveTile(id, place));

    /// <summary>Moves the selected tile by whole cells (the keyboard path for dragging).</summary>
    public Task<bool> MoveSelectedAsync(int rows, int columns) =>
        SelectedTile is { } tile
            ? MoveTileAsync(tile.Id, tile.Place with { Row = tile.Place.Row + rows, Column = tile.Place.Column + columns })
            : Task.FromResult(false);

    /// <summary>Grows or shrinks the selected tile by whole cells (the keyboard path for resizing).</summary>
    public Task<bool> ResizeSelectedAsync(int width, int height) =>
        SelectedTile is { } tile
            ? MoveTileAsync(tile.Id, tile.Place with { Width = tile.Place.Width + width, Height = tile.Place.Height + height })
            : Task.FromResult(false);

    /// <summary>Changes the selected tile's title and whether it follows the shared filters.</summary>
    public Task<bool> SetTileSettingsAsync(string title, bool usesFilters) =>
        SelectedTile is { } tile
            ? ApplyAsync(new DashboardOp.SetTile(tile.Id, title, usesFilters))
            : Task.FromResult(false);

    /// <summary>Adds a filter shared by the tiles.</summary>
    public Task<bool> AddSharedFilterAsync(SharedFilter filter) => ApplyAsync(new DashboardOp.AddFilter(filter));

    /// <summary>Removes a shared filter.</summary>
    public Task<bool> RemoveSharedFilterAsync(string name) => ApplyAsync(new DashboardOp.RemoveFilter(name));

    /// <summary>Sets the refresh policy.</summary>
    public Task<bool> SetRefreshAsync(RefreshPolicy policy) => ApplyAsync(new DashboardOp.SetRefresh(policy));

    /// <summary>Shares the dashboard with the roles that may open it.</summary>
    public Task<bool> ShareAsync(IReadOnlyList<string> roles) => ApplyAsync(new DashboardOp.SetRoles(roles));

    /// <summary>Applies one op; on a refusal the draft stays as it was and the status says why.</summary>
    public async Task<bool> ApplyAsync(DashboardOp op)
    {
        ArgumentNullException.ThrowIfNull(op);
        return await Guard.RunAsync(async () =>
        {
            var outcome = await _server.ApplyDashboardAsync(_draftId, op, _version).ConfigureAwait(true);
            if (!outcome.Ok)
            {
                Status = outcome.Refusal is { } refusal ? $"{refusal.Message} {refusal.Remedy}".Trim() : "That change was not accepted.";
                return false;
            }

            Take(outcome);
            await RefreshAsync().ConfigureAwait(true);
            return true;
        }, message => Status = message).ConfigureAwait(true);
    }

    /// <summary>Validates and refreshes the bounded preview of every tile.</summary>
    public async Task RefreshAsync(CancellationToken cancellationToken = default)
    {
        if (!ParameterInput.TryCollect(FilterInputs, out var values, out var problem))
        {
            Status = problem;
            return;
        }

        // Typed values win; a filter left empty is cleared; values set directly on FilterValues for
        // a name without an input stay as they are.
        foreach (var input in FilterInputs)
        {
            if (values.TryGetValue(input.Name, out var value))
            {
                FilterValues[input.Name] = value;
            }
            else
            {
                FilterValues.Remove(input.Name);
            }
        }

        await Guard.RunAsync(async () =>
        {
            var issues = await _server.ValidateDashboardAsync(_draftId, cancellationToken).ConfigureAwait(true);
            Issues.Clear();
            foreach (var issue in issues)
            {
                Issues.Add(issue);
            }

            OnPropertyChanged(nameof(CanPublish));
            PublishCommand.RaiseCanExecuteChanged();
            TilePreviews.Clear();
            if (Draft is { Tiles.Count: > 0 })
            {
                var previews = await _server.PreviewDashboardAsync(_draftId, FilterValues, _previewRows, cancellationToken).ConfigureAwait(true);
                foreach (var preview in previews)
                {
                    TilePreviews.Add(preview);
                }
            }

            return true;
        }, message => Status = message).ConfigureAwait(true);
    }

    /// <summary>The first place, scanning row by row, where a tile of this size fits without overlap.</summary>
    public Place? FirstFreePlace(int width, int height)
    {
        if (Draft is null)
        {
            return null;
        }

        for (var row = 1; row + height - 1 <= Draft.GridRows; row++)
        {
            for (var column = 1; column + width - 1 <= Draft.GridColumns; column++)
            {
                var candidate = new Place(row, column, width, height);
                if (Draft.Tiles.All(t => !t.Place.Overlaps(candidate)))
                {
                    return candidate;
                }
            }
        }

        return null;
    }

    /// <summary>
    /// The place a drag is over, as the grid draws the footprint; null when no drag is in progress.
    /// </summary>
    /// <remarks>
    /// This is a hint for the person dragging, never a decision: <see cref="MoveTileAsync"/> still
    /// asks the server, which refuses a place that overlaps or falls off the grid.
    /// </remarks>
    public Place? DropTarget
    {
        get => _dropTarget;
        private set
        {
            if (SetField(ref _dropTarget, value))
            {
                OnPropertyChanged(nameof(HasDropTarget));
            }
        }
    }

    /// <summary>Whether a drag is in progress.</summary>
    public bool HasDropTarget => DropTarget is not null;

    /// <summary>Whether the place under a drag is free: on the grid and not over another tile.</summary>
    public bool DropTargetIsFree
    {
        get => _dropTargetIsFree;
        private set => SetField(ref _dropTargetIsFree, value);
    }

    /// <summary>Shows where a dragged tile would land and whether that place is free.</summary>
    /// <param name="place">The snapped place, or null to forget the drag.</param>
    /// <param name="movingTileId">The tile being dragged, so its own cells do not count as taken.</param>
    public void ShowDropTarget(Place? place, string? movingTileId = null)
    {
        DropTarget = place;
        DropTargetIsFree = place is not null && IsFreePlace(place, movingTileId);
    }

    /// <summary>Forgets the drag (a drop, an escape, or the grid being redrawn).</summary>
    public void ClearDropTarget() => ShowDropTarget(null);

    /// <summary>Whether a tile at this place would sit on the grid without covering another tile.</summary>
    /// <param name="place">The place to test.</param>
    /// <param name="movingTileId">The tile being moved, if any; its own cells are not an obstacle.</param>
    public bool IsFreePlace(Place place, string? movingTileId = null)
    {
        ArgumentNullException.ThrowIfNull(place);
        if (Draft is not { } draft)
        {
            return false;
        }

        var onGrid = place.Row >= 1 && place.Column >= 1
            && place.LastRow <= draft.GridRows && place.LastColumn <= draft.GridColumns;
        return onGrid && draft.Tiles.All(t => t.Id == movingTileId || !t.Place.Overlaps(place));
    }

    private async Task PublishAsync()
    {
        var result = await _server.PublishDashboardAsync(_draftId).ConfigureAwait(true);
        PublishMessage = result.Message;
        Status = result.Ok ? "Published." : result.Message;
    }

    private string NextTileId(TileKind kind)
    {
        var prefix = BuilderJson.Snake(kind);
        var n = 1;
        var taken = Draft?.Tiles ?? (IReadOnlyList<Tile>)Array.Empty<Tile>();
        while (taken.Any(t => t.Id == $"{prefix}_{n}"))
        {
            n++;
        }

        return $"{prefix}_{n}";
    }

    private void Take(DashboardOutcome outcome)
    {
        Draft = outcome.Draft;
        _version = outcome.Version;
        Status = string.Empty;
        ClearDropTarget(); // a new draft ends any drag that was in flight
        OnPropertyChanged(nameof(SelectedTile));
        OnPropertyChanged(nameof(GridColumns));
        OnPropertyChanged(nameof(GridRows));
    }

    private void SyncFilterInputs()
    {
        var wanted = _draft?.Filters ?? (IReadOnlyList<SharedFilter>)Array.Empty<SharedFilter>();
        if (wanted.Select(f => f.Name).SequenceEqual(FilterInputs.Select(f => f.Name)))
        {
            return;
        }

        FilterInputs.Clear();
        foreach (var filter in wanted)
        {
            FilterInputs.Add(new ParameterInput(filter.Name, filter.Label, filter.Type, filter.Default));
        }
    }

    private void SyncTileSettings()
    {
        if (SelectedTile is { } tile)
        {
            TileTitleEdit = tile.Title;
            TileUsesFiltersEdit = tile.UsesFilters;
        }
        else
        {
            TileTitleEdit = string.Empty;
            TileUsesFiltersEdit = true;
        }
    }
}
