using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Workspace;

/// <summary>Timing and threading of autosave.</summary>
public sealed record AutosaveOptions
{
    /// <summary>Write this long after the last edit. Default 2 seconds.</summary>
    public TimeSpan Debounce { get; init; } = TimeSpan.FromSeconds(2);

    /// <summary>Write at least this often while edits keep coming. Default 15 seconds.</summary>
    public TimeSpan MaxInterval { get; init; } = TimeSpan.FromSeconds(15);

    /// <summary>Runs an action on the UI thread (timers fire elsewhere). Default: run it where the timer fired.</summary>
    public Action<Action>? Post { get; init; }
}

/// <summary>What one open form tells the coordinator beyond its draft.</summary>
public sealed record TrackOptions
{
    /// <summary>The title for the recovery list.</summary>
    public Func<string?>? Title { get; init; }

    /// <summary>The form's tab state, stored with the draft.</summary>
    public Func<string?>? TabState { get; init; }

    /// <summary>True while a save request of the form is in flight; autosave never writes then.</summary>
    public Func<bool>? IsSaving { get; init; }

    /// <summary>The new-draft id to keep using (a recovered new document keeps writing to its own file).</summary>
    public string? NewDraftId { get; init; }
}

/// <summary>What autosave last did, for a quiet status-bar indicator.</summary>
public enum AutosaveState
{
    /// <summary>Nothing written yet.</summary>
    Idle,

    /// <summary>The last write went to disk.</summary>
    Saved,

    /// <summary>The last write is in memory only.</summary>
    MemoryOnly,

    /// <summary>The last write failed; the next edit tries again.</summary>
    Failed,
}

/// <summary>The autosave status. <see cref="Message"/> is plain text for the status bar, never a dialog.</summary>
/// <param name="State">What happened.</param>
/// <param name="Message">A short line, or null.</param>
/// <param name="AtUtc">When.</param>
public sealed record AutosaveStatus(AutosaveState State, string? Message, DateTimeOffset AtUtc);

/// <summary>
/// Writes drafts while people edit. It plugs into the form through <see cref="IDraftAutosave"/>:
/// each change restarts a debounce timer (2 s) and the first change after a write starts a maximum
/// timer (15 s), so a long burst of typing is still written. It also writes on tab deactivate and app
/// deactivate (<see cref="FlushAsync"/>, <see cref="FlushAllAsync"/>), never while the form has a save
/// request in flight, and keeps one pending write per draft. A failure only changes <see cref="Status"/>.
/// </summary>
public sealed class AutosaveCoordinator : IDraftAutosave, IDisposable
{
    private readonly IDraftStore _store;
    private readonly DraftScope _scope;
    private readonly TimeProvider _clock;
    private readonly AutosaveOptions _options;
    private readonly Dictionary<DocumentDraft, Entry> _entries = new(ReferenceEqualityComparer.Instance);
    private readonly object _lock = new();
    private bool _disposed;

    /// <summary>Creates the coordinator for one signed-in user on one server.</summary>
    public AutosaveCoordinator(IDraftStore store, DraftScope scope, TimeProvider? clock = null, AutosaveOptions? options = null)
    {
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _scope = scope?.Validated() ?? throw new ArgumentNullException(nameof(scope));
        _clock = clock ?? TimeProvider.System;
        _options = options ?? new AutosaveOptions();
        Status = new AutosaveStatus(AutosaveState.Idle, null, _clock.GetUtcNow());
    }

    /// <summary>What autosave last did.</summary>
    public AutosaveStatus Status { get; private set; }

    /// <summary>Raised when <see cref="Status"/> changes.</summary>
    public event EventHandler? StatusChanged;

    /// <summary>The scope this coordinator writes for.</summary>
    public DraftScope Scope => _scope;

    /// <summary>Registers a form's draft with its title, tab state and save gate. Optional: an untracked draft is tracked with defaults on its first change.</summary>
    public void Track(DocumentDraft draft, string doctype, TrackOptions? options = null)
    {
        ArgumentNullException.ThrowIfNull(draft);
        _ = GetEntry(draft, doctype, options);
    }

    /// <inheritdoc />
    public void DraftChanged(string doctype, string? documentId, DocumentDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        var entry = GetEntry(draft, doctype, null);
        lock (_lock)
        {
            if (_disposed)
            {
                return;
            }

            entry.Dirty = true;
            entry.FirstDirtyAt ??= _clock.GetUtcNow();
            entry.MaxTimer ??= _clock.CreateTimer(_ => Fire(entry), null, _options.MaxInterval, Timeout.InfiniteTimeSpan);
            entry.DebounceTimer ??= _clock.CreateTimer(_ => Fire(entry), null, Timeout.InfiniteTimeSpan, Timeout.InfiniteTimeSpan);
            _ = entry.DebounceTimer.Change(_options.Debounce, Timeout.InfiniteTimeSpan);
        }
    }

    /// <inheritdoc />
    public void DraftSettled(string doctype, string? documentId)
    {
        Entry[] hit;
        lock (_lock)
        {
            hit = [.. _entries.Values.Where(e => e.DocType == doctype && (documentId is null || e.Draft.DocumentId == documentId || e.LastKey?.DocumentId == documentId))];
        }

        foreach (var entry in hit)
        {
            _ = SettleAsync(entry);
        }
    }

    /// <summary>Writes the draft now if it has unwritten changes (the tab was deactivated or is closing).</summary>
    public Task FlushAsync(DocumentDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        Entry? entry;
        lock (_lock)
        {
            _ = _entries.TryGetValue(draft, out entry);
        }

        return entry is { Dirty: true } ? WriteAsync(entry, force: false) : Task.CompletedTask;
    }

    /// <summary>Writes every draft with unwritten changes (the app was deactivated or is closing).</summary>
    public Task FlushAllAsync()
    {
        Entry[] dirty;
        lock (_lock)
        {
            dirty = [.. _entries.Values.Where(e => e.Dirty)];
        }

        return Task.WhenAll(dirty.Select(e => WriteAsync(e, force: false)));
    }

    /// <summary>Stops tracking a form's draft after writing any last change (the form closed with unsaved edits; the draft stays on disk).</summary>
    public async Task UntrackAsync(DocumentDraft draft)
    {
        await FlushAsync(draft).ConfigureAwait(false);
        lock (_lock)
        {
            if (_entries.Remove(draft, out var entry))
            {
                entry.StopTimers();
            }
        }
    }

    /// <summary>Deletes the stored draft and stops tracking it (the user chose Discard, or the form saved).</summary>
    public async Task DiscardAsync(DocumentDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        Entry? entry;
        lock (_lock)
        {
            _ = _entries.TryGetValue(draft, out entry);
        }

        if (entry is not null)
        {
            await SettleAsync(entry).ConfigureAwait(false);
        }
    }

    /// <summary>The key a tracked draft is stored under now; null when it is not tracked.</summary>
    public DraftKey? KeyOf(DocumentDraft draft)
    {
        lock (_lock)
        {
            return _entries.TryGetValue(draft, out var entry) ? entry.CurrentKey(_scope) : null;
        }
    }

    /// <summary>
    /// The user signed out: stops all writing and removes every draft of this user on this server,
    /// and the key that protected them. Drafts do not survive a sign-out.
    /// </summary>
    public async Task SignOutAsync()
    {
        Dispose();
        await _store.DeleteAllAsync(_scope).ConfigureAwait(false);
    }

    /// <inheritdoc />
    public void Dispose()
    {
        lock (_lock)
        {
            _disposed = true;
            foreach (var entry in _entries.Values)
            {
                entry.StopTimers();
            }

            _entries.Clear();
        }
    }

    // ------------------------------------------------------------------ internals

    private Entry GetEntry(DocumentDraft draft, string doctype, TrackOptions? options)
    {
        lock (_lock)
        {
            if (!_entries.TryGetValue(draft, out var entry))
            {
                entry = new Entry(draft, doctype, options ?? new TrackOptions());
                _entries[draft] = entry;
            }
            else if (options is not null)
            {
                entry.Options = options;
                entry.NewDraftId = options.NewDraftId ?? entry.NewDraftId;
            }

            return entry;
        }
    }

    private void Fire(Entry entry)
    {
        void Run() => _ = WriteAsync(entry, force: false);
        if (_options.Post is { } post)
        {
            post(Run);
        }
        else
        {
            Run();
        }
    }

    private async Task SettleAsync(Entry entry)
    {
        Task? inFlight;
        DraftKey? last;
        lock (_lock)
        {
            entry.Dirty = false;
            entry.FirstDirtyAt = null;
            entry.StopTimers();
            inFlight = entry.InFlight;
            last = entry.LastKey;
        }

        if (inFlight is not null)
        {
            try
            {
                await inFlight.ConfigureAwait(false);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                // The write's own failure is reported by the write.
            }
        }

        var keys = new HashSet<DraftKey> { entry.CurrentKey(_scope) };
        if (last is not null)
        {
            _ = keys.Add(last);
        }

        if (entry.NewDraftId is { } fresh)
        {
            _ = keys.Add(new DraftKey(_scope, entry.DocType, null, fresh));
        }

        foreach (var key in keys)
        {
            await _store.DeleteAsync(key).ConfigureAwait(false);
        }
    }

    private async Task WriteAsync(Entry entry, bool force)
    {
        Task? running;
        lock (_lock)
        {
            if (_disposed || (!entry.Dirty && !force))
            {
                return;
            }

            if (entry.Options.IsSaving?.Invoke() == true)
            {
                // Never write during a save request: try again after the debounce.
                entry.DebounceTimer?.Change(_options.Debounce, Timeout.InfiniteTimeSpan);
                return;
            }

            if (entry.InFlight is { IsCompleted: false })
            {
                entry.Pending = true;
                return;
            }

            entry.StopTimers();
            entry.Dirty = false;
            entry.FirstDirtyAt = null;
            running = entry.InFlight = RunAsync(entry);
        }

        await running.ConfigureAwait(false);
    }

    private async Task RunAsync(Entry entry)
    {
        do
        {
            DraftRecord record;
            DraftKey key;
            DraftKey? previous;
            lock (_lock)
            {
                entry.Pending = false;
                key = entry.CurrentKey(_scope);
                previous = entry.LastKey;
                entry.LastKey = key;
                record = DraftSnapshot.Capture(entry.Draft, key, _clock.GetUtcNow(), entry.Options.Title?.Invoke(), entry.Options.TabState?.Invoke());
            }

            try
            {
                if (previous is not null && !previous.Equals(key))
                {
                    await _store.DeleteAsync(previous).ConfigureAwait(false);
                }

                if (record.IsEmpty)
                {
                    await _store.DeleteAsync(key).ConfigureAwait(false);
                    continue;
                }

                var result = await _store.SaveAsync(record).ConfigureAwait(false);
                Report(result);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                SetStatus(new AutosaveStatus(AutosaveState.Failed, "Draft not saved: " + ex.GetType().Name + ".", _clock.GetUtcNow()));
                lock (_lock)
                {
                    entry.Dirty = true;
                }
            }
        }
        while (entry.Pending);
    }

    private void Report(DraftSaveResult result)
    {
        var state = result.Outcome switch
        {
            DraftSaveOutcome.Saved => AutosaveState.Saved,
            DraftSaveOutcome.SavedInMemoryOnly => AutosaveState.MemoryOnly,
            _ => AutosaveState.Failed,
        };
        var message = result.Outcome == DraftSaveOutcome.SavedInMemoryOnly ? FileDraftStore.MemoryOnlyMessage : result.Message;
        SetStatus(new AutosaveStatus(state, message, _clock.GetUtcNow()));
    }

    private void SetStatus(AutosaveStatus status)
    {
        Status = status;
        StatusChanged?.Invoke(this, EventArgs.Empty);
    }

    private sealed class Entry(DocumentDraft draft, string docType, TrackOptions options)
    {
        public DocumentDraft Draft { get; } = draft;
        public string DocType { get; } = docType;
        public TrackOptions Options { get; set; } = options;
        public string? NewDraftId { get; set; } = options.NewDraftId ?? Guid.NewGuid().ToString("N");
        public bool Dirty { get; set; }
        public bool Pending { get; set; }
        public DateTimeOffset? FirstDirtyAt { get; set; }
        public ITimer? DebounceTimer { get; set; }
        public ITimer? MaxTimer { get; set; }
        public Task? InFlight { get; set; }
        public DraftKey? LastKey { get; set; }

        public DraftKey CurrentKey(DraftScope scope)
            => Draft.DocumentId is { } id ? DraftKey.ForDocument(scope, DocType, id) : new DraftKey(scope, DocType, null, NewDraftId);

        public void StopTimers()
        {
            DebounceTimer?.Dispose();
            MaxTimer?.Dispose();
            DebounceTimer = null;
            MaxTimer = null;
        }
    }
}
