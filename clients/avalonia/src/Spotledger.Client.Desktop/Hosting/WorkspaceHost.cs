using System.ComponentModel;
using Avalonia.Threading;
using Spotledger.Client.Forms;
using Spotledger.Client.Shell;
using Spotledger.Client.Workspace;

namespace Spotledger.Client.Desktop.Hosting;

/// <summary>
/// Wires the keyboard-first workspace into the desktop shell (WP-UI-17.T4): a draft saved for every
/// edited form, the recovery bar, one undo history per form, Edit &gt; Undo and Redo, File &gt; Recover
/// drafts, and the wipe on sign-out. It adds no rule of its own: the autosave coordinator, the undo
/// stack and the recovery bar are the Workspace types, the gates are the form's, and nothing here sends
/// anything to the server (only a form's explicit Save does).
/// </summary>
internal sealed class WorkspaceHost : IDisposable
{
    internal const string RecoverKey = "workspace:recover-drafts";

    private readonly ShellViewModel _shell;
    private readonly DraftSessionHost _drafts;
    private readonly IDocumentGateway _gateway;
    private readonly Func<string> _serverId;
    private readonly TimeProvider _clock;
    private readonly Action<Action>? _undoSchedule;
    private readonly Action<Action> _post;
    private readonly Dictionary<DocumentFormViewModel, OpenForm> _forms = [];
    private int _draftCount;
    private string _shownUndo = "Undo";
    private string _shownRedo = "Redo";
    private string _shownRecover = "Recover drafts";
    private bool _memoryNoticeShown;
    private bool _disposed;

    private sealed class OpenForm(AutosaveCoordinator coordinator, IDraftStore store, DraftScope scope)
    {
        public AutosaveCoordinator Coordinator { get; } = coordinator;
        public IDraftStore Store { get; } = store;
        public DraftScope Scope { get; } = scope;
        public WorkspaceTabViewModel? Tab { get; set; }
        public IDisposable? Binding { get; set; }
        public FormHistoryAdapter? History { get; set; }
        public EventHandler? HistoryChanged { get; set; }
    }

    public WorkspaceHost(
        ShellViewModel shell,
        DraftSessionHost drafts,
        IDocumentGateway gateway,
        Func<string> serverId,
        TimeProvider? clock = null,
        Action<Action>? post = null,
        Action<Action>? undoSchedule = null)
    {
        _shell = shell ?? throw new ArgumentNullException(nameof(shell));
        _drafts = drafts ?? throw new ArgumentNullException(nameof(drafts));
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        _serverId = serverId ?? throw new ArgumentNullException(nameof(serverId));
        _clock = clock ?? TimeProvider.System;
        _undoSchedule = undoSchedule;
        _post = post ?? (action => Dispatcher.UIThread.Post(action));

        _shell.AddCommands(WorkspaceCommands.Register);
        _shell.ExtendKeymaps(profile => WorkspaceCommands.Extend(profile, _shell.Registry));
        _shell.WrapAvailability(inner => WorkspaceCommands.Availability(RecoverAvailability(inner), ActiveStack));
        _shell.CommandLabelProvider = LabelOf;
        _shell.Dispatcher.Register(WorkspaceCommands.UndoId, _ => Step(undo: true));
        _shell.Dispatcher.Register(WorkspaceCommands.RedoId, _ => Step(undo: false));
        _shell.Dispatcher.Register(WorkspaceCommands.RecoverDraftsId, _ => OpenRecoverDraftsAsync());
        _shell.TabClosed += OnTabClosed;
        _shell.SignedOut += OnSignedOut;
        _shell.PropertyChanged += OnShellChanged;
        _drafts.Notice += _shell.ShowStatus;
        _drafts.ScopeChanged += OnScopeChanged;
    }

    /// <summary>How many drafts the open scope holds (the File menu text and the status line show it).</summary>
    public int DraftCount => _draftCount;

    /// <summary>The form of the selected tab, when the tab holds one.</summary>
    public DocumentFormViewModel? ActiveForm => _shell.SelectedTab?.Content as DocumentFormViewModel;

    /// <summary>Opens a document (or a new one) in its tab. <paramref name="newDraftId"/> reopens a new document's stored draft.</summary>
    public WorkspaceTabViewModel? OpenDocument(string doctype, string? id, string? newDraftId = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        if (_drafts.Coordinator is not { } coordinator || _drafts.Store is not { } store || _drafts.Scope is not { } scope)
        {
            _shell.ShowStatus("Sign in to open documents.");
            return null;
        }

        var draftId = id is null ? newDraftId ?? Guid.NewGuid().ToString("N") : null;
        var key = id is null ? $"document:{doctype}:new:{draftId}" : WorkspaceTabKinds.DocumentKey(doctype, id);
        if (_shell.Tabs.FirstOrDefault(t => t.Key == key) is { } open)
        {
            _shell.SelectTab(open);
            return open;
        }

        var info = new OpenForm(coordinator, store, scope);
        var form = new DocumentFormViewModel(_gateway, doctype, id, new HostFormShell(this), coordinator)
        {
            HistoryFactory = draft => CreateHistory(info, draft),
            Opened = f => OpenedAsync(f, info, draftId),
        };
        _forms[form] = info;
        var tab = _shell.OpenContentTab(form.Title, form, key);
        info.Tab = tab;
        info.Binding = DocumentTabs.Bind(tab, form);
        tab.CloseGuard = () => GuardCloseAsync(tab, form, info);
        _ = LoadAsync(form);
        return tab;
    }

    /// <summary>Opens the Recover drafts list as a singleton tab.</summary>
    public async Task OpenRecoverDraftsAsync()
    {
        if (_drafts.Store is not { } store || _drafts.Scope is not { } scope)
        {
            return;
        }

        var model = _shell.Tabs.FirstOrDefault(t => t.Key == RecoverKey)?.Content as RecoverDraftsViewModel
            ?? new RecoverDraftsViewModel(store, scope, OpenDraftAsync);
        await model.LoadAsync();
        _ = _shell.OpenContentTab("Recover drafts", model, RecoverKey);
        await RefreshCountAsync();
    }

    /// <summary>Counts the open scope's drafts again and refreshes the File menu text.</summary>
    public async Task RefreshCountAsync()
    {
        var count = 0;
        if (_drafts.Store is { } store && _drafts.Scope is { } scope)
        {
            try
            {
                count = (await store.ListAsync(scope)).Count;
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                count = 0;
            }
        }

        _draftCount = count;
        UpdateLabels();
    }

    /// <summary>The drafts left by an earlier run, announced quietly on the status bar (never a dialog).</summary>
    public async Task AnnounceDraftsAsync()
    {
        await RefreshCountAsync();
        if (_draftCount > 0)
        {
            _shell.ShowStatus(_draftCount == 1
                ? "1 unsaved draft was kept. File > Recover drafts lists it."
                : $"{_draftCount} unsaved drafts were kept. File > Recover drafts lists them.");
        }
    }

    /// <summary>Writes the drafts with unwritten changes (the window was deactivated or is closing).</summary>
    public Task FlushAllAsync() => _drafts.FlushAllAsync();

    /// <summary>Writes the draft of one tab (the tab was deactivated).</summary>
    public Task FlushAsync(WorkspaceTabViewModel tab)
        => tab.Content is DocumentFormViewModel { Draft: { } draft } form && _forms.TryGetValue(form, out var info)
            ? info.Coordinator.FlushAsync(draft)
            : Task.CompletedTask;

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _shell.TabClosed -= OnTabClosed;
        _shell.SignedOut -= OnSignedOut;
        _shell.PropertyChanged -= OnShellChanged;
        _drafts.Notice -= _shell.ShowStatus;
        _drafts.ScopeChanged -= OnScopeChanged;
    }

    // ------------------------------------------------------------------ forms

    private FormHistoryAdapter CreateHistory(OpenForm info, Spotledger.Client.Metadata.DocumentDraft draft)
    {
        var adapter = new FormHistoryAdapter(new UndoStack(draft, _clock, schedule: _undoSchedule));
        info.History = adapter;
        info.HistoryChanged = (_, _) => UpdateLabels();
        adapter.Changed += info.HistoryChanged;
        return adapter;
    }

    private async Task LoadAsync(DocumentFormViewModel form)
    {
        try
        {
            await form.LoadAsync();
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _shell.ShowStatus("The document could not be opened: " + ex.GetType().Name + ".");
        }
    }

    /// <summary>After a load: track the draft for autosave and offer a stored draft. Nothing is applied or sent here.</summary>
    private static async Task OpenedAsync(DocumentFormViewModel form, OpenForm info, string? newDraftId)
    {
        if (form.Draft is not { } draft)
        {
            return;
        }

        info.Coordinator.Track(draft, form.DocType, new TrackOptions
        {
            Title = () => form.Title,
            IsSaving = () => form.IsBusy,
            NewDraftId = newDraftId,
        });

        RecoveryBarViewModel? bar;
        if (draft.DocumentId is not null)
        {
            bar = await RecoveryBarViewModel.TryCreateAsync(
                info.Store, info.Scope, form.DocType, draft, info.Coordinator, null, () => form.IsReadOnly);
        }
        else
        {
            var record = newDraftId is null ? null : await info.Store.LoadAsync(new DraftKey(info.Scope, form.DocType, null, newDraftId));
            bar = record is null || record.IsEmpty
                ? null
                : new RecoveryBarViewModel(info.Store, record, draft, info.Coordinator, null, () => form.IsReadOnly);
        }

        if (bar is not null)
        {
            form.Recovery = new RecoveryBarView { DataContext = bar };
        }
    }

    private async Task<bool> GuardCloseAsync(WorkspaceTabViewModel tab, DocumentFormViewModel form, OpenForm info)
    {
        if (!form.IsDirty)
        {
            return true;
        }

        var choice = _shell.ClosePromptRequested is { } ask ? await ask(tab) : TabCloseChoice.Cancel;
        switch (choice)
        {
            case TabCloseChoice.Save:
                return form.CommitEditors?.Invoke() != false && await form.SaveAsync();
            case TabCloseChoice.Discard:
                if (form.Draft is { } draft)
                {
                    await info.Coordinator.DiscardAsync(draft);
                }

                return true;
            default:
                return false;
        }
    }

    private Task OpenDraftAsync(DraftKey key)
    {
        _ = OpenDocument(key.DocType, key.DocumentId, key.NewDraftId);
        return Task.CompletedTask;
    }

    private void OnTabClosed(object? sender, TabClosedEventArgs e)
    {
        switch (e.Tab.Content)
        {
            case DocumentFormViewModel form when _forms.Remove(form, out var info):
                _ = CloseFormAsync(form, info, e.SignedOut);
                break;
        }
    }

    private async Task CloseFormAsync(DocumentFormViewModel form, OpenForm info, bool signedOut)
    {
        info.Binding?.Dispose();
        if (info.History is { } history && info.HistoryChanged is { } handler)
        {
            history.Changed -= handler;
        }

        try
        {
            if (!signedOut && form.Draft is { } draft)
            {
                // The form closed with unsaved edits: the draft stays on disk (the crash and restart case).
                await info.Coordinator.UntrackAsync(draft);
            }
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _shell.ShowStatus("A draft could not be saved: " + ex.GetType().Name + ".");
        }
        finally
        {
            form.Dispose();
        }

        UpdateLabels();
    }

    // ------------------------------------------------------------------ session

    private void OnShellChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(ShellViewModel.UserId):
                _ = SwitchToUserAsync();
                break;
            case nameof(ShellViewModel.SelectedTab):
                UpdateLabels();
                break;
        }
    }

    private async Task SwitchToUserAsync()
    {
        var user = _shell.UserId;
        var server = _serverId();
        if (user.Length == 0 || string.IsNullOrWhiteSpace(server))
        {
            // The session ended: the SignedOut handler wipes this scope. Flushing it here would keep it.
            return;
        }

        var next = new DraftScope(server, user);
        if (!Equals(next, _drafts.Scope))
        {
            // Forms of the previous scope close first; each flushes to its own coordinator.
            foreach (var tab in _shell.Tabs.Where(t => t.Content is DocumentFormViewModel f && _forms.TryGetValue(f, out var i) && !Equals(i.Scope, next)).ToList())
            {
                _shell.CloseTab(tab);
            }
        }

        await _drafts.SwitchAsync(next);
        await _drafts.RetryPendingWipesAsync();
        await AnnounceDraftsAsync();
    }

    private async void OnSignedOut(object? sender, EventArgs e)
    {
        try
        {
            await _drafts.SignOutAsync();
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _shell.ShowStatus("Unsaved drafts could not be removed: " + ex.GetType().Name + ".");
        }

        await RefreshCountAsync();
    }

    private void OnScopeChanged(object? sender, EventArgs e)
    {
        if (_drafts.Coordinator is { } coordinator)
        {
            coordinator.StatusChanged += OnAutosaveStatus;
        }

        _ = RefreshCountAsync();
    }

    private void OnAutosaveStatus(object? sender, EventArgs e) => _post(() =>
    {
        if (_drafts.Coordinator is { Status: { State: AutosaveState.MemoryOnly, Message: { } message } } && !_memoryNoticeShown)
        {
            _memoryNoticeShown = true;
            _shell.ShowStatus(message);
        }

        _ = RefreshCountAsync();
    });

    // ------------------------------------------------------------------ commands

    private UndoStack? ActiveStack() => (ActiveForm?.History as FormHistoryAdapter)?.Stack;

    private AvailabilityProvider RecoverAvailability(AvailabilityProvider? inner) => (definition, context) =>
    {
        var answer = inner?.Invoke(definition, context) ?? CommandAvailability.Available;
        if (definition.Id != WorkspaceCommands.RecoverDraftsId || !answer.IsAvailable)
        {
            return answer;
        }

        return _drafts.Scope is null ? CommandAvailability.Blocked("Sign in to recover drafts.") : CommandAvailability.Available;
    };

    private Task Step(bool undo)
    {
        if (ActiveForm?.History is { } history)
        {
            var result = undo ? history.Undo() : history.Redo();
            if (!result.Succeeded)
            {
                // A refusal changed nothing; say why instead of staying silent.
                _shell.ShowStatus(result.Message ?? "That change cannot be applied now.");
            }
        }

        UpdateLabels();
        return Task.CompletedTask;
    }

    private string? LabelOf(CommandDefinition definition) => definition.Id switch
    {
        WorkspaceCommands.UndoId => WorkspaceCommands.UndoText(ActiveStack()),
        WorkspaceCommands.RedoId => WorkspaceCommands.RedoText(ActiveStack()),
        WorkspaceCommands.RecoverDraftsId => _draftCount > 0 ? $"Recover drafts ({_draftCount})" : null,
        _ => null,
    };

    /// <summary>Rebuilds the menu when a label it shows changed (and only then).</summary>
    private void UpdateLabels()
    {
        var undo = WorkspaceCommands.UndoText(ActiveStack());
        var redo = WorkspaceCommands.RedoText(ActiveStack());
        var recover = _draftCount > 0 ? $"Recover drafts ({_draftCount})" : "Recover drafts";
        if (undo == _shownUndo && redo == _shownRedo && recover == _shownRecover)
        {
            _shell.RefreshCommandState();
            return;
        }

        (_shownUndo, _shownRedo, _shownRecover) = (undo, redo, recover);
        _shell.RefreshMenu();
        _shell.RefreshCommandState();
    }

    /// <summary>What a form asks of its host: open another document, close its tab. Questions with a dialog stay declined.</summary>
    private sealed class HostFormShell(WorkspaceHost host) : IFormShell
    {
        public Task<bool> ConfirmAsync(string title, string message, string confirmLabel) => Task.FromResult(false);

        public Task<string?> AskTextAsync(string title, string prompt, bool required) => Task.FromResult<string?>(null);

        public Task<bool> ConfirmDiscardAsync(IReadOnlyList<string> changed) => Task.FromResult(false);

        public Task<ConflictChoice> ChooseConflictAsync(ConflictInfo info) => Task.FromResult(ConflictChoice.Keep);

        public Task ShowReferrersAsync(string title, IReadOnlyList<Referrer> referrers) => Task.CompletedTask;

        public void OpenDocument(string doctype, string? id) => host.OpenDocument(doctype, id);

        public void CloseTab()
        {
            if (host.ActiveForm is { } form && host._forms.TryGetValue(form, out var info) && info.Tab is { } tab)
            {
                host._shell.CloseTab(tab);
            }
        }
    }
}
