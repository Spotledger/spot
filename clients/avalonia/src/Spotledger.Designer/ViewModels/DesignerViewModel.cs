using System.Collections.ObjectModel;

namespace Spotledger.Client.Designer;

/// <summary>
/// The designer as one view-model: the palette, the canvas, the inspector, the validation panel,
/// the preview, the link picker and the publish dialog, plus the draft lifecycle (autosave,
/// undo/redo against the server's op log, history, discard and publish).
/// </summary>
/// <remarks>
/// Every path — pointer drag, keyboard gesture or a command from a panel — goes through
/// <see cref="DesignerSession"/> and so becomes a server op. That is what lets the headless tests
/// build the golden 'Vehicle Service Request' design with UI commands and compare the ops the
/// designer issued, one for one, with the server-side golden sequence.
/// </remarks>
public sealed class DesignerViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private string _statusText = "Open a draft to start designing.";
    private string _lastCommandText = string.Empty;
    private string? _selectedPaletteType;
    private string _newSectionTitle = "Section";

    /// <summary>Creates the designer over a session.</summary>
    /// <param name="session">The draft the designer edits.</param>
    /// <param name="tools">
    /// The tools the composition root registered (the report builder, the dashboard builder, the
    /// approval wizard). The shell only reads them: with none, the designer is exactly the DocType
    /// designer it was before.
    /// </param>
    /// <param name="toolContext">
    /// The connection and item the tools are given. The host passes the connection its tools adapt
    /// (one app client usually carries both surfaces); by default the designer's own connection is
    /// used, which is right for a tool that talks to the designer server itself.
    /// </param>
    public DesignerViewModel(DesignerSession session, DesignerToolRegistry? tools = null, DesignerToolContext? toolContext = null)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        Tools = new DesignerToolListViewModel(
            tools ?? new DesignerToolRegistry(),
            toolContext ?? new DesignerToolContext(_session.Server, _session.DraftId));
        Canvas = new CanvasViewModel(_session);
        Inspector = new InspectorViewModel(_session);
        Validation = new ValidationPanelViewModel(_session);
        Preview = new PreviewViewModel(_session);
        Publish = new PublishViewModel(_session);
        LinkPicker = new LinkPickerViewModel(_session);
        FieldDialog = new FieldDialogViewModel(_session);
        ChildTableDialog = new ChildTableDialogViewModel(_session);
        UndoCommand = new AsyncRelayCommand(_ => _session.UndoAsync());
        RedoCommand = new AsyncRelayCommand(_ => _session.RedoAsync());
        SaveCommand = new AsyncRelayCommand(_ => _session.SaveAsync());
        DiscardCommand = new AsyncRelayCommand(_ => _session.DiscardAsync());
        PublishDialogCommand = new AsyncRelayCommand(_ => Publish.OpenAsync());
        InsertCommand = new AsyncRelayCommand(_ => InsertSelectedPaletteTypeAsync(), _ => SelectedPaletteType is not null);
        DeleteCommand = new AsyncRelayCommand(_ => Canvas.RemoveSelectedAsync(), _ => Canvas.SelectedField is not null);
        AddSectionCommand = new AsyncRelayCommand(_ => Canvas.AddSectionAsync(NewSectionTitle));
        Canvas.SelectionChanged += (_, _) =>
        {
            SyncInspector();
            DeleteCommand.RaiseCanExecuteChanged();
        };
        Validation.NavigateRequested += (_, issue) => NavigateTo(issue);
        LinkPicker.Chosen += (_, result) => StatusText = result.Ok ? DescribeLastCommand() : result.Message ?? "The change was refused.";
        ChildTableDialog.Closed += (_, result) =>
        {
            if (result.Ok)
            {
                Canvas.Rebuild();
                SelectLastTouchedField();
                StatusText = DescribeLastCommand();
            }
            else if (result.Message is not null)
            {
                StatusText = result.Message;
            }
        };
        FieldDialog.Closed += (_, result) =>
        {
            if (result.Ok)
            {
                Canvas.Rebuild();
                SelectLastTouchedField();
                StatusText = DescribeLastCommand();
            }
            else if (result.Message is not null)
            {
                StatusText = result.Message;
            }
        };
        _session.Changed += (_, _) => Refresh();
    }

    /// <summary>The command layer.</summary>
    public DesignerSession Session => _session;

    /// <summary>
    /// The other tools the host registered beside the DocType designer: the list, the one that is
    /// open, and the sentence to show when one could not be created.
    /// </summary>
    public DesignerToolListViewModel Tools { get; }

    /// <summary>The canvas.</summary>
    public CanvasViewModel Canvas { get; }

    /// <summary>The inspector.</summary>
    public InspectorViewModel Inspector { get; }

    /// <summary>The validation panel.</summary>
    public ValidationPanelViewModel Validation { get; }

    /// <summary>The preview pane.</summary>
    public PreviewViewModel Preview { get; }

    /// <summary>The publish dialog.</summary>
    public PublishViewModel Publish { get; }

    /// <summary>The link picker.</summary>
    public LinkPickerViewModel LinkPicker { get; }

    /// <summary>The child-table dialog.</summary>
    public ChildTableDialogViewModel ChildTableDialog { get; }

    /// <summary>The field dialog a palette drop opens (label and storage name).</summary>
    public FieldDialogViewModel FieldDialog { get; }

    /// <summary>The palette entry the keyboard's insert gesture uses.</summary>
    public string? SelectedPaletteType
    {
        get => _selectedPaletteType;
        set
        {
            if (SetField(ref _selectedPaletteType, value))
            {
                InsertCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>Inserts the selected palette type at the selection (Ctrl+I's command form).</summary>
    public AsyncRelayCommand InsertCommand { get; }

    /// <summary>Removes the selected field.</summary>
    public AsyncRelayCommand DeleteCommand { get; }

    /// <summary>Adds a section titled <see cref="NewSectionTitle"/>.</summary>
    public AsyncRelayCommand AddSectionCommand { get; }

    /// <summary>The title of the next section the user adds.</summary>
    public string NewSectionTitle
    {
        get => _newSectionTitle;
        set => SetField(ref _newSectionTitle, value);
    }

    /// <summary>The server's op log, for the history list.</summary>
    public ObservableCollection<HistoryEntry> History { get; } = new();

    /// <summary>Moves the server's op-log cursor back one op.</summary>
    public AsyncRelayCommand UndoCommand { get; }

    /// <summary>Moves the server's op-log cursor forward one op.</summary>
    public AsyncRelayCommand RedoCommand { get; }

    /// <summary>Saves the draft now.</summary>
    public AsyncRelayCommand SaveCommand { get; }

    /// <summary>Discards the draft.</summary>
    public AsyncRelayCommand DiscardCommand { get; }

    /// <summary>Builds the publish plan and shows the dialog.</summary>
    public AsyncRelayCommand PublishDialogCommand { get; }

    /// <summary>The DocType's label, for the designer's title.</summary>
    public string Title => _session.Draft is { } draft ? $"{draft.Label} ({draft.Doctype})" : "No draft";

    /// <summary>What the designer is doing, or the last refusal.</summary>
    public string StatusText
    {
        get => _statusText;
        private set => SetField(ref _statusText, value);
    }

    /// <summary>The last op the designer issued, described.</summary>
    public string LastCommandText
    {
        get => _lastCommandText;
        private set => SetField(ref _lastCommandText, value);
    }

    /// <summary>How the autosave is doing, for the status bar.</summary>
    public string SaveStateText => _session.SaveState switch
    {
        SaveState.Saved => "Saved",
        SaveState.Saving => "Saving…",
        SaveState.Failed => "Not saved",
        _ => string.Empty,
    };

    /// <summary>Whether undo would move back in the server's op log.</summary>
    public bool CanUndo => (_session.Draft?.UndoDepth ?? 0) > 0;

    /// <summary>Whether redo would move forward in the server's op log.</summary>
    public bool CanRedo => (_session.Draft?.RedoDepth ?? 0) > 0;

    /// <summary>Whether the design can be published as far as the server's permission goes.</summary>
    public bool CanPublish => _session.Draft?.CanPublish ?? false;

    /// <summary>The number of ops the designer has issued.</summary>
    public int IssuedCommandCount => _session.Sent.Count;

    /// <summary>Opens the draft and fills every panel.</summary>
    public async Task<DesignerResult> OpenAsync()
    {
        var result = await _session.OpenAsync().ConfigureAwait(true);
        Refresh();
        if (!result.Ok)
        {
            StatusText = result.Message ?? "The draft could not be opened.";
        }

        return result;
    }

    /// <summary>
    /// Drops a palette type on the canvas. A link asks the server for the DocType choices first,
    /// and a child table opens the dialog that names it, so the field always points at something
    /// that exists and the grid is named before it is added.
    /// </summary>
    public Task<DesignerResult> DropAsync(string type, string sectionId, string? afterField = null)
    {
        var info = FieldTypeCatalog.Find(type) ?? throw new ArgumentException($"`{type}` is not in the palette.", nameof(type));
        var placement = string.IsNullOrEmpty(sectionId) ? null : new DesignerPlacement(sectionId, afterField);
        if (info.NeedsTarget)
        {
            LinkPicker.Open(info.Title, placement);
            StatusText = "Search for the DocType to link to.";
            return Task.FromResult(DesignerResult.Success);
        }

        if (info.IsChildTable)
        {
            ChildTableDialog.Open(placement, ChildTableLabelGuess());
            StatusText = "Name the child table and the DocType its rows are stored in.";
            return Task.FromResult(DesignerResult.Success);
        }

        FieldDialog.Open(info, placement, Canvas.ScopeChildDoctype);
        StatusText = $"Name the {info.Title.ToLowerInvariant()} field.";
        return Task.FromResult(DesignerResult.Success);
    }

    /// <summary>Adds the palette's selected type at the selection (the button next to Ctrl+I).</summary>
    public Task<DesignerResult> InsertSelectedPaletteTypeAsync()
    {
        if (SelectedPaletteType is not { } type)
        {
            StatusText = "Pick a field type in the palette first.";
            return Task.FromResult(new DesignerResult(false, null, StatusText));
        }

        var sectionId = Canvas.SelectedSectionId ?? string.Empty;
        return DropAsync(type, sectionId, SelectedFieldInScope());
    }

    /// <summary>Adds a child table whose rows come from an existing DocType (the picker's route).</summary>
    public async Task<DesignerResult> AddChildTableFromExistingAsync(string childDoctype, string childLabel, string sectionId, string? afterField = null)
    {
        var result = await _session.AddChildTableAsync(
            FieldTypeCatalog.TitleOf("table"),
            childDoctype,
            childLabel,
            new DesignerPlacement(sectionId, afterField)).ConfigureAwait(true);
        Refresh();
        StatusText = result.Ok ? DescribeLastCommand() : result.Message ?? "The change was refused.";
        return result;
    }

    /// <summary>
    /// The keyboard route for the canvas: <paramref name="gesture"/> names a key chord
    /// (<c>Alt+Up</c>, <c>Ctrl+I</c>) and <paramref name="paletteType"/> is what "insert" inserts.
    /// </summary>
    public async Task<(bool Handled, DesignerResult Result)> HandleGestureAsync(string gesture, string? paletteType = null)
    {
        // An insert is a palette drop: it opens the same field dialog, so the keyboard and the
        // pointer produce the same op with the same label.
        if (CanvasKeyboardMap.Resolve(gesture) == CanvasGesture.Insert)
        {
            var type = paletteType ?? SelectedPaletteType;
            if (type is null)
            {
                StatusText = "Pick a field type in the palette first.";
                return (true, new DesignerResult(false, null, StatusText));
            }

            var dropped = await DropAsync(type, Canvas.SelectedSectionId ?? string.Empty, SelectedFieldInScope()).ConfigureAwait(true);
            return (true, dropped);
        }

        var (handled, result) = await Canvas.HandleGestureAsync(gesture, paletteType ?? SelectedPaletteType).ConfigureAwait(true);
        if (handled)
        {
            Refresh();
            StatusText = result.Ok ? DescribeLastCommand() : result.Message ?? "The change was refused.";
        }

        return (handled, result);
    }

    /// <summary>Selects a field on the canvas, as a validation issue's click does.</summary>
    public void SelectField(string name, string? childDoctype = null)
    {
        if (childDoctype is not null && !string.Equals(Canvas.ScopeChildDoctype, childDoctype, StringComparison.Ordinal))
        {
            Canvas.OpenChildGrid(childDoctype);
        }

        var match = Canvas.CurrentSections.SelectMany(s => s.Fields).Concat(Canvas.CurrentUnplaced)
            .FirstOrDefault(f => string.Equals(f.Name, name, StringComparison.Ordinal));
        if (match is not null)
        {
            Canvas.SelectedField = match;
        }
    }

    /// <summary>Selects a section on the canvas (the keyboard actions act on it).</summary>
    public void SelectSection(string sectionId) => Canvas.SelectedSectionId = sectionId;

    /// <summary>Reloads every panel from the server's draft.</summary>
    public void Refresh()
    {
        Canvas.Rebuild();
        Validation.Rebuild();
        History.Clear();
        if (_session.Draft is { } draft)
        {
            foreach (var entry in draft.Log)
            {
                History.Add(entry);
            }
        }

        SyncInspector();
        OnPropertyChanged(nameof(Title));
        OnPropertyChanged(nameof(SaveStateText));
        OnPropertyChanged(nameof(CanUndo));
        OnPropertyChanged(nameof(CanRedo));
        OnPropertyChanged(nameof(CanPublish));
        OnPropertyChanged(nameof(IssuedCommandCount));
    }

    private void SyncInspector()
    {
        var scope = Canvas.CurrentSections.SelectMany(s => s.Fields).Concat(Canvas.CurrentUnplaced).ToList();
        Inspector.Inspect(Canvas.SelectedField, scope);
    }

    private void NavigateTo(ValidationIssue issue)
    {
        if (!string.IsNullOrEmpty(issue.Section))
        {
            SelectSection(issue.Section);
        }

        if (!string.IsNullOrEmpty(issue.Field))
        {
            SelectField(issue.Field, FindOwningChild(issue));
        }

        StatusText = $"{issue.SeverityText}: {issue.Message}";
    }

    private string? FindOwningChild(ValidationIssue issue)
    {
        if (_session.Draft is not { } draft || string.Equals(issue.DocType, draft.Doctype, StringComparison.Ordinal))
        {
            return null;
        }

        return draft.ChildList.Any(c => string.Equals(c.Doctype, issue.DocType, StringComparison.Ordinal))
            ? issue.DocType
            : null;
    }

    /// <summary>
    /// Selects the field the last op added or moved, so the next insert (a key or a drop) lands
    /// next to what the user just created instead of at the end of the section.
    /// </summary>
    private void SelectLastTouchedField()
    {
        if (_session.Sent.Count == 0)
        {
            return;
        }

        var name = _session.Sent[^1] switch
        {
            AddFieldOp add => add.Name,
            MoveInLayoutOp move => move.Field,
            AddLinkOp link => link.Name,
            AddChildTableOp table => table.Name,
            RenameFieldOp rename => rename.To,
            _ => null,
        };
        if (name is not null)
        {
            SelectField(name);
        }
    }

    /// <summary>The selected field's name when it belongs to the canvas's current scope.</summary>
    private string? SelectedFieldInScope() =>
        Canvas.SelectedField is { } field && string.Equals(field.ChildDoctype, Canvas.ScopeChildDoctype, StringComparison.Ordinal)
            ? field.Name
            : null;

    private string ChildTableLabelGuess() => _session.Draft is { Label.Length: > 0 } draft ? $"{draft.Label} item" : "Child table";

    private string DescribeLastCommand()
    {
        var last = _session.Sent.Count > 0 ? _session.Sent[^1] : null;
        if (last is null)
        {
            return "Nothing changed.";
        }

        LastCommandText = DesignerOpText.Describe(last);
        return LastCommandText;
    }
}
