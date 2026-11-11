using System.Collections.ObjectModel;
using System.Text.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Forms;

/// <summary>Where a form is in its life.</summary>
public enum FormPhase
{
    /// <summary>Metadata, permissions and the document are being fetched.</summary>
    Loading,

    /// <summary>The draft is ready to show and edit.</summary>
    Ready,

    /// <summary>Loading failed; <see cref="DocumentFormViewModel.LoadError"/> says why.</summary>
    Failed,
}

/// <summary>One toolbar action, with whether it can run now and, when it cannot, why.</summary>
/// <param name="Id"><c>save</c>, <c>submit</c>, <c>cancel</c>, <c>correct</c>, <c>delete</c>, <c>rename</c> or <c>new</c>.</param>
/// <param name="Label">The button text.</param>
/// <param name="IsEnabled">Whether the permission summary and the document state allow it. The server still decides.</param>
/// <param name="DisabledReason">One sentence for a disabled action (a tooltip), else null.</param>
/// <param name="Gesture">The shortcut, for the tooltip.</param>
public sealed record FormAction(string Id, string Label, bool IsEnabled, string? DisabledReason, string? Gesture);

/// <summary>The audit footer: who created and last changed the document, and when. Empty parts are null.</summary>
public sealed record AuditFooter(string? CreatedBy, string? CreatedAt, string? ModifiedBy, string? ModifiedAt)
{
    /// <summary>True when the server sent none of it (a new document).</summary>
    public bool IsEmpty => CreatedBy is null && CreatedAt is null && ModifiedBy is null && ModifiedAt is null;

    internal static AuditFooter From(string documentJson)
    {
        using var document = JsonDocument.Parse(documentJson);
        string? Pick(params string[] keys)
        {
            foreach (var key in keys)
            {
                if (document.RootElement.TryGetProperty(key, out var value) && value.ValueKind == JsonValueKind.String)
                {
                    return value.GetString();
                }
            }

            return null;
        }

        return new AuditFooter(
            Pick("_created_by", "created_by", "owner"),
            Pick("_created_at", "created_at", "creation"),
            Pick("_modified_by", "modified_by", "last_modified_by"),
            Pick("_modified_at", "modified_at", "modified"));
    }
}

/// <summary>
/// One open document: loads it with its metadata and permissions, holds the draft, saves with
/// merge patches and optimistic concurrency, runs the lifecycle actions the permission summary and
/// the document state allow, and guards unsaved changes. It contains no DocType-specific code and
/// decides no permission beyond hiding what the summary hides: every operation is still the
/// server's to accept or refuse.
/// </summary>
public sealed partial class DocumentFormViewModel : ObservableObject, IDisposable
{
    private readonly IDocumentGateway _gateway;
    private readonly string _doctype;
    private readonly CancellationTokenSource _lifetime = new();
    private DocumentDraft? _draft;
    private FormLayout? _layout;
    private FormPhase _phase = FormPhase.Loading;
    private string? _loadError;
    private DocumentRights _rights = DocumentRights.None;
    private bool _busy;
    private ConflictInfo? _conflict;
    private AuditFooter _audit = new(null, null, null, null);
    private int _messageSerial;
    private IFormHistory? _history;
    private object? _recovery;
    private string? _historyNote;

    /// <summary>Creates a form for <paramref name="doctype"/>; <paramref name="documentId"/> null opens a new document.</summary>
    public DocumentFormViewModel(
        IDocumentGateway gateway,
        string doctype,
        string? documentId = null,
        IFormShell? shell = null,
        IDraftAutosave? autosave = null)
    {
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        _doctype = doctype;
        RequestedId = documentId;
        Shell = shell ?? new NullFormShell();
        Autosave = autosave;
    }

    /// <summary>The DocType's storage name.</summary>
    public string DocType => _doctype;

    /// <summary>The resolved view to lay the form out with (tabs of sections); null shows the
    /// generated default in field order. A host sets it before <see cref="LoadAsync"/>.</summary>
    public FormView? ViewDefinition { get; set; }

    /// <summary>The id this form was asked to open (null for a new document).</summary>
    public string? RequestedId { get; }

    /// <summary>The dialogs and tab operations of the host.</summary>
    public IFormShell Shell { get; }

    /// <summary>The autosave seam (WP-UI-17); null means no autosave.</summary>
    public IDraftAutosave? Autosave { get; }

    /// <summary>Builds the form's undo history over a freshly loaded draft (WP-UI-17). The host
    /// sets it before <see cref="LoadAsync"/>; null means the form has no undo.</summary>
    public Func<DocumentDraft, IFormHistory?>? HistoryFactory { get; set; }

    /// <summary>Called when a load has completed and the form is ready: the host tracks the draft for
    /// autosave and offers a stored draft here. A failure is reported as a message, never thrown.</summary>
    public Func<DocumentFormViewModel, Task>? Opened { get; set; }

    /// <summary>The form's undo and redo history; null until loaded or when the host gave none.</summary>
    public IFormHistory? History => _history;

    /// <summary>The non-modal slot above the form body (the recovery bar); null when empty.</summary>
    public object? Recovery
    {
        get => _recovery;
        set => SetField(ref _recovery, value);
    }

    /// <summary>The note the history left when it was cleared by a save, reload or revert; null until then.</summary>
    public string? HistoryNote
    {
        get => _historyNote;
        private set => SetField(ref _historyNote, value);
    }

    /// <summary>Set by the view: the check of one child table by field name (the table's own
    /// editable check). Until the view sets it every table step is refused.</summary>
    public Func<string, ChildTable.ChildTableResult?>? TableGate { get; set; }

    /// <summary>Whether a cancel asks for a reason first. The metadata does not say, so the host
    /// decides per DocType; the default asks for none.</summary>
    public Func<DocTypeModel, bool> RequiresCancelReason { get; set; } = static _ => false;

    /// <summary>Set by the view: commits every open editor into the draft, returning false when a
    /// value does not parse (the editor then shows its own message).</summary>
    public Func<bool>? CommitEditors { get; set; }

    /// <summary>The form's phase.</summary>
    public FormPhase Phase
    {
        get => _phase;
        private set => SetField(ref _phase, value);
    }

    /// <summary>Why loading failed, when it did.</summary>
    public string? LoadError
    {
        get => _loadError;
        private set => SetField(ref _loadError, value);
    }

    /// <summary>The draft; null until the form is ready.</summary>
    public DocumentDraft? Draft => _draft;

    /// <summary>The resolved layout of the draft; rebuilt when an instant rule changes what is shown.</summary>
    public FormLayout? Layout => _layout;

    /// <summary>The DocType model; null until loaded.</summary>
    public DocTypeModel? Model => _draft?.Model;

    /// <summary>The signed-in user's document rights on this DocType.</summary>
    public DocumentRights Rights => _rights;

    /// <summary>The saved document's id; null while the document is new.</summary>
    public string? DocumentId => _draft?.DocumentId;

    /// <summary>True while the document has not been saved yet.</summary>
    public bool IsNew => _draft is not null && _draft.DocumentId is null;

    /// <summary>The document's own state.</summary>
    public DocumentStatus Status => _draft?.Status ?? DocumentStatus.Unknown;

    /// <summary>True while a request is in flight; actions are disabled meanwhile.</summary>
    public bool IsBusy
    {
        get => _busy;
        private set
        {
            if (SetField(ref _busy, value))
            {
                RaiseActionsChanged();
            }
        }
    }

    /// <summary>True when the draft holds changes that are not saved.</summary>
    public bool IsDirty => _draft?.IsDirty == true;

    /// <summary>The version conflict waiting for a decision, or null.</summary>
    public ConflictInfo? Conflict
    {
        get => _conflict;
        private set
        {
            if (SetField(ref _conflict, value))
            {
                RaiseActionsChanged();
            }
        }
    }

    /// <summary>The audit footer of the loaded document.</summary>
    public AuditFooter Audit
    {
        get => _audit;
        private set => SetField(ref _audit, value);
    }

    /// <summary>Messages from the server and from this form, newest last.</summary>
    public ObservableCollection<FormMessage> Messages { get; } = [];

    /// <summary>Raised when an instant rule changed what the layout shows; the view rebuilds.</summary>
    public event EventHandler? LayoutChanged;

    /// <summary>Raised when a saved or created document got an id (the tab key changes).</summary>
    public event EventHandler? DocumentIdChanged;

    /// <summary>Raised to put the focus on the first field that carries an error.</summary>
    public event EventHandler<string>? FocusRequested;

    /// <summary>Raised when field errors were attached or cleared; the view refreshes the messages beside the fields.</summary>
    public event EventHandler? ErrorsChanged;

    /// <summary>Raised when the form wants its tab closed (after a delete).</summary>
    public event EventHandler? CloseRequested;

    /// <summary>Whether the document cannot be edited: no write right, posted or cancelled.</summary>
    public bool IsReadOnly
    {
        get
        {
            if (_draft is null)
            {
                return true;
            }

            if (IsNew)
            {
                return !_rights.Create;
            }

            return !_rights.Write || Status is DocumentStatus.Posted or DocumentStatus.Cancelled;
        }
    }

    /// <summary>The badge text for the document's state.</summary>
    public string StatusBadge
    {
        get
        {
            if (_draft is null)
            {
                return string.Empty;
            }

            if (IsNew)
            {
                return "Not saved";
            }

            return Status switch
            {
                DocumentStatus.Draft => IsDirty ? "Draft, modified" : "Draft",
                DocumentStatus.Posted => "Submitted",
                DocumentStatus.Cancelled => "Cancelled",
                _ => IsDirty ? "Modified" : "Saved",
            };
        }
    }

    /// <summary>The tab and heading title: the DocType label with the document's title, else its id.</summary>
    public string Title
    {
        get
        {
            if (_draft is null)
            {
                return _doctype;
            }

            var label = _draft.Model.Label;
            if (IsNew)
            {
                return "New " + label;
            }

            return $"{label} {TitleText() ?? DocumentId}";
        }
    }

    /// <summary>The tab's unique key: one tab per (DocType, id).</summary>
    public string TabKey => DocumentTabs.KeyFor(_doctype, DocumentId ?? RequestedId);

    /// <summary>Loads the metadata, the permission summary and (for an existing document) the document.</summary>
    public async Task LoadAsync(CancellationToken cancellationToken = default)
    {
        Phase = FormPhase.Loading;
        LoadError = null;
        try
        {
            using var linked = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, _lifetime.Token);
            var token = linked.Token;
            var model = await _gateway.GetMetaAsync(_doctype, token);
            var permissions = await _gateway.GetPermissionsAsync(_doctype, token);
            DocumentReply? reply = null;
            if (RequestedId is not null)
            {
                reply = await _gateway.LoadAsync(_doctype, RequestedId, token);
            }

            var draft = new DocumentDraft(model, RequestedId);
            draft.ApplyPermissions(permissions.Snapshot);
            if (reply is not null)
            {
                draft.LoadFrom(reply.DataJson);
                Audit = AuditFooter.From(reply.DataJson);
                ReplaceMessages(reply.Messages);
            }

            Attach(draft);
            _rights = permissions.Rights;
            Rebuild();
            Phase = FormPhase.Ready;
            RaiseAll();
            await NotifyOpenedAsync();
        }
        catch (ApiRequestException e)
        {
            LoadError = e.Error.Message;
            Phase = FormPhase.Failed;
        }
        catch (OperationCanceledException)
        {
            LoadError = "Loading was cancelled.";
            Phase = FormPhase.Failed;
        }
        catch (Exception e) when (e is JsonException or InvalidOperationException or ArgumentException)
        {
            LoadError = "The document could not be read: " + e.Message;
            Phase = FormPhase.Failed;
        }
    }

    /// <summary>Stops everything this form has in flight (the tab was closed).</summary>
    public void Dispose()
    {
        _lifetime.Cancel();
        _lifetime.Dispose();
        DetachHistory();
        if (_draft is not null)
        {
            _draft.FieldChanged -= OnDraftChanged;
            _draft.StructureChanged -= OnStructureChanged;
            _draft.Rules.RulesChanged -= OnRulesChanged;
        }
    }

    // ------------------------------------------------------------------ draft plumbing

    private void Attach(DocumentDraft draft)
    {
        if (_draft is not null)
        {
            _draft.FieldChanged -= OnDraftChanged;
            _draft.StructureChanged -= OnStructureChanged;
            _draft.Rules.RulesChanged -= OnRulesChanged;
        }

        _draft = draft;
        AttachHistory(draft);
        draft.FieldChanged += OnDraftChanged;
        draft.StructureChanged += OnStructureChanged;
        draft.Rules.RulesChanged += OnRulesChanged;
    }

    private void Rebuild()
    {
        if (_draft is null)
        {
            return;
        }

        _layout = FormLayout.Build(_draft.Model, _draft, ViewDefinition);
        OnPropertyChanged(nameof(Layout));
        LayoutChanged?.Invoke(this, EventArgs.Empty);
    }

    private void OnDraftChanged(object? sender, string field)
    {
        RaiseDirty();
        if (_draft is not null)
        {
            Autosave?.DraftChanged(_doctype, _draft.DocumentId, _draft);
        }
    }

    private void OnStructureChanged(object? sender, StructureChangedEventArgs e)
    {
        if (e.Kind is StructureChangeKind.DocumentLoaded or StructureChangeKind.Saved)
        {
            return;
        }

        RaiseDirty();
        if (_draft is not null)
        {
            Autosave?.DraftChanged(_doctype, _draft.DocumentId, _draft);
        }
    }

    private void OnRulesChanged(object? sender, RulesChangedEventArgs e) => Rebuild();

    private void RaiseDirty()
    {
        OnPropertyChanged(nameof(IsDirty));
        OnPropertyChanged(nameof(StatusBadge));
        OnPropertyChanged(nameof(Title));
        RaiseActionsChanged();
    }

    private void RaiseAll()
    {
        OnPropertyChanged(nameof(Draft));
        OnPropertyChanged(nameof(Model));
        OnPropertyChanged(nameof(Rights));
        OnPropertyChanged(nameof(DocumentId));
        OnPropertyChanged(nameof(IsNew));
        OnPropertyChanged(nameof(Status));
        OnPropertyChanged(nameof(IsReadOnly));
        OnPropertyChanged(nameof(TabKey));
        RaiseDirty();
    }

    private string? TitleText()
    {
        if (_draft is null)
        {
            return null;
        }

        // The metadata carries no title field: the first required text field of the layout names
        // the document (a generic rule, not a DocType-specific one); the id is the fallback.
        foreach (var tab in _layout?.Tabs ?? [])
        {
            foreach (var section in tab.Sections)
            {
                foreach (var field in section.Fields)
                {
                    if (field.Required && field.Kind == FieldKind.Text
                        && _draft.Get(field.Name) is TextFieldValue { Value.Length: > 0 } text)
                    {
                        return text.Value;
                    }
                }
            }
        }

        return null;
    }

    private void ReplaceMessages(IReadOnlyList<FormMessage> messages)
    {
        Messages.Clear();
        foreach (var message in messages)
        {
            Messages.Add(message);
        }

        _messageSerial++;
    }

    private void AddMessage(string level, string? code, string text) => Messages.Add(new FormMessage(level, code, text));

    /// <summary>Takes a server answer into the draft: server-derived fields appear, the draft is clean.</summary>
    private void ApplyReply(DocumentReply reply)
    {
        var draft = _draft ?? throw new InvalidOperationException("The form is not loaded.");
        var hadId = draft.DocumentId;
        draft.LoadFrom(reply.DataJson);
        Audit = AuditFooter.From(reply.DataJson);
        ReplaceMessages(reply.Messages);
        Conflict = null;
        Autosave?.DraftSettled(_doctype, draft.DocumentId);
        Rebuild();
        RaiseAll();
        ErrorsChanged?.Invoke(this, EventArgs.Empty);
        if (hadId != draft.DocumentId)
        {
            DocumentIdChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>Applies an API error to the draft and the message area; returns the first field to focus.</summary>
    private ErrorApplication ShowError(ApiError error)
    {
        var draft = _draft ?? throw new InvalidOperationException("The form is not loaded.");
        var applied = ErrorMapper.Apply(draft, error);
        if (!applied.IsStaleVersion)
        {
            foreach (var message in applied.DocumentMessages)
            {
                AddMessage("error", message.Code, message.Message);
            }

            FocusFirstInvalid();
        }

        RaiseDirty();
        ErrorsChanged?.Invoke(this, EventArgs.Empty);
        return applied;
    }

    /// <summary>Puts the focus on the first field of the layout that carries an error.</summary>
    public void FocusFirstInvalid()
    {
        if (_draft is null || _layout is null)
        {
            return;
        }

        foreach (var tab in _layout.Tabs)
        {
            foreach (var section in tab.Sections)
            {
                foreach (var field in section.Fields)
                {
                    var invalid = _draft.Fields.TryGetValue(field.Name, out var state) && state.Errors.Count > 0;
                    if (!invalid && field.Kind == FieldKind.Table && _draft.Tables.TryGetValue(field.Name, out var table))
                    {
                        invalid = table.Rows.Any(row => row.RowErrors.Count > 0
                            || row.Values.Values.Any(value => value.Errors.Count > 0));
                    }

                    if (invalid)
                    {
                        FocusRequested?.Invoke(this, field.Name);
                        return;
                    }
                }
            }
        }
    }

    private void RequestClose() => CloseRequested?.Invoke(this, EventArgs.Empty);
}

public sealed partial class DocumentFormViewModel
{
    /// <summary>The lookup source of a link field of the document, built from the draft's current
    /// snapshot (never from a stale one); null when the host has none.</summary>
    public Controls.ILookupSource? LookupFor(FieldModel field)
    {
        var draft = _draft;
        return draft is null ? null : _gateway.LookupFor(_doctype, field.Name, draft.LookupSnapshot);
    }

    /// <summary>The lookup source of a link cell of a child table row.</summary>
    public Controls.ILookupSource? LookupFor(ChildTable.ChildTableLookupRequest request)
    {
        var draft = _draft;
        var rowDoctype = draft?.Model.ChildTable(request.Table)?.Name;
        return draft is null || rowDoctype is null
            ? null
            : _gateway.LookupFor(rowDoctype, request.Field.Name, () => draft.LookupSnapshot(request.Table, request.RowIndex + 1));
    }
}
