using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

/// <summary>A store that counts, can block a save, and can fail.</summary>
internal sealed class SpyStore : IDraftStore
{
    private readonly InMemoryDraftStore _inner = new();
    private int _running;

    public List<DraftRecord> Saves { get; } = [];
    public int MaxConcurrent { get; private set; }
    public TaskCompletionSource? Block { get; set; }
    public DraftSaveOutcome Outcome { get; set; } = DraftSaveOutcome.Saved;

    public string? MemoryOnlyNotice => null;
    public IReadOnlyList<QuarantineNote> Quarantined => [];
    public event EventHandler<QuarantineNote>? DraftQuarantined { add { } remove { } }

    public Task<bool> IsPersistentAsync(DraftScope scope, CancellationToken cancellationToken = default) => Task.FromResult(true);

    public async Task<DraftSaveResult> SaveAsync(DraftRecord record, CancellationToken cancellationToken = default)
    {
        MaxConcurrent = Math.Max(MaxConcurrent, Interlocked.Increment(ref _running));
        try
        {
            if (Block is { } block)
            {
                await block.Task;
            }

            if (Outcome is DraftSaveOutcome.Failed or DraftSaveOutcome.TooLarge)
            {
                return new DraftSaveResult(Outcome, "disk full");
            }

            Saves.Add(record);
            _ = await _inner.SaveAsync(record, cancellationToken);
            return new DraftSaveResult(Outcome);
        }
        finally
        {
            _ = Interlocked.Decrement(ref _running);
        }
    }

    public Task<DraftRecord?> LoadAsync(DraftKey key, CancellationToken cancellationToken = default) => _inner.LoadAsync(key, cancellationToken);
    public Task DeleteAsync(DraftKey key, CancellationToken cancellationToken = default) => _inner.DeleteAsync(key, cancellationToken);
    public Task<IReadOnlyList<DraftSummary>> ListAsync(DraftScope scope, CancellationToken cancellationToken = default) => _inner.ListAsync(scope, cancellationToken);
    public Task DeleteAllAsync(DraftScope scope, CancellationToken cancellationToken = default) => _inner.DeleteAllAsync(scope, cancellationToken);
}

public sealed class AutosaveTests
{
    private readonly ManualClock _clock = new();
    private readonly SpyStore _store = new();

    private AutosaveCoordinator Coordinator() => new(_store, Models.Scope, _clock);

    private static void Wire(AutosaveCoordinator coordinator, DocumentDraft draft)
        => draft.FieldChanged += (_, _) => coordinator.DraftChanged("invoice", draft.DocumentId, draft);

    [Fact]
    public void Writes_two_seconds_after_the_last_edit()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        Wire(coordinator, draft);

        draft.Set("title", new TextFieldValue("a"));
        _clock.Advance(TimeSpan.FromSeconds(1.9));
        Assert.Empty(_store.Saves);
        draft.Set("title", new TextFieldValue("ab"));
        _clock.Advance(TimeSpan.FromSeconds(1.9));
        Assert.Empty(_store.Saves);
        _clock.Advance(TimeSpan.FromSeconds(0.2));

        var saved = Assert.Single(_store.Saves);
        Assert.Contains(saved.Fields, f => f.Name == "title" && f.Json == "\"ab\"");
        Assert.Equal(AutosaveState.Saved, coordinator.Status.State);
        Assert.Equal(0, _clock.ActiveTimers);
    }

    [Fact]
    public void Writes_at_least_every_fifteen_seconds_while_edits_keep_coming()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        Wire(coordinator, draft);

        for (var second = 1; second <= 14; second++)
        {
            draft.Set("title", new TextFieldValue("t" + second));
            _clock.Advance(TimeSpan.FromSeconds(1));
        }

        Assert.Empty(_store.Saves);
        draft.Set("title", new TextFieldValue("t15"));
        _clock.Advance(TimeSpan.FromSeconds(1));

        var saved = Assert.Single(_store.Saves);
        Assert.Contains(saved.Fields, f => f.Json == "\"t15\"" || f.Json == "\"t14\"");
    }

    [Fact]
    public void Never_writes_while_a_save_request_is_in_flight()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        var saving = true;
        coordinator.Track(draft, "invoice", new TrackOptions { IsSaving = () => saving });
        Wire(coordinator, draft);

        draft.Set("title", new TextFieldValue("a"));
        _clock.Advance(TimeSpan.FromSeconds(10));
        Assert.Empty(_store.Saves);

        saving = false;
        _clock.Advance(TimeSpan.FromSeconds(2.1));
        Assert.Single(_store.Saves);
    }

    [Fact]
    public async Task Keeps_one_pending_write_per_draft()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        Wire(coordinator, draft);
        _store.Block = new TaskCompletionSource();

        draft.Set("title", new TextFieldValue("one"));
        var first = coordinator.FlushAsync(draft);
        draft.Set("title", new TextFieldValue("two"));
        var second = coordinator.FlushAsync(draft);
        draft.Set("title", new TextFieldValue("three"));
        var third = coordinator.FlushAsync(draft);
        Assert.False(first.IsCompleted);

        _store.Block.SetResult();
        await Task.WhenAll(first, second, third);

        Assert.Equal(1, _store.MaxConcurrent);
        Assert.Equal(2, _store.Saves.Count);
        Assert.Contains(_store.Saves[^1].Fields, f => f.Json == "\"three\"");
    }

    [Fact]
    public async Task A_failure_is_a_quiet_status_not_an_exception_or_a_dialog()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        Wire(coordinator, draft);
        var changes = 0;
        coordinator.StatusChanged += (_, _) => changes++;
        _store.Outcome = DraftSaveOutcome.Failed;

        draft.Set("title", new TextFieldValue("x"));
        await coordinator.FlushAsync(draft);

        Assert.Equal(AutosaveState.Failed, coordinator.Status.State);
        Assert.Equal("disk full", coordinator.Status.Message);
        Assert.Equal(1, changes);

        _store.Outcome = DraftSaveOutcome.Saved;
        draft.Set("title", new TextFieldValue("y"));
        await coordinator.FlushAsync(draft);
        Assert.Equal(AutosaveState.Saved, coordinator.Status.State);
    }

    [Fact]
    public async Task Memory_only_storage_is_reported_with_the_notice()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        Wire(coordinator, draft);
        _store.Outcome = DraftSaveOutcome.SavedInMemoryOnly;

        draft.Set("title", new TextFieldValue("x"));
        await coordinator.FlushAsync(draft);

        Assert.Equal(AutosaveState.MemoryOnly, coordinator.Status.State);
        Assert.Equal("Drafts are not saved to disk on this machine.", coordinator.Status.Message);
    }

    [Fact]
    public async Task Tab_and_app_deactivation_write_at_once()
    {
        using var coordinator = Coordinator();
        var a = Models.LoadedInvoice();
        var b = Models.LoadedInvoice();
        Wire(coordinator, a);
        Wire(coordinator, b);
        a.Set("title", new TextFieldValue("a"));
        b.Set("title", new TextFieldValue("b"));

        await coordinator.FlushAsync(a);
        Assert.Single(_store.Saves);
        await coordinator.FlushAllAsync();
        Assert.Equal(2, _store.Saves.Count);
        await coordinator.FlushAllAsync();
        Assert.Equal(2, _store.Saves.Count);
    }

    [Fact]
    public async Task A_saved_or_discarded_draft_is_removed_and_a_reverted_one_is_not_kept()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        Wire(coordinator, draft);
        draft.Set("title", new TextFieldValue("x"));
        await coordinator.FlushAsync(draft);
        Assert.Single(await _store.ListAsync(Models.Scope));

        // The form saved: the stored copy goes.
        draft.MarkSaved();
        coordinator.DraftSettled("invoice", "INV-1");
        await coordinator.FlushAllAsync();
        Assert.Empty(await _store.ListAsync(Models.Scope));

        // An edit that is put back to the saved value leaves nothing to keep.
        draft.Set("title", new TextFieldValue("y"));
        await coordinator.FlushAsync(draft);
        Assert.Single(await _store.ListAsync(Models.Scope));
        draft.Set("title", draft.Fields["title"].Original);
        await coordinator.FlushAsync(draft);
        Assert.Empty(await _store.ListAsync(Models.Scope));
    }

    [Fact]
    public async Task A_new_document_is_kept_under_its_own_id_and_moves_to_the_document_id_once_saved()
    {
        using var coordinator = Coordinator();
        var draft = new DocumentDraft(Models.Invoice());
        Wire(coordinator, draft);
        draft.Set("title", new TextFieldValue("brand new"));
        await coordinator.FlushAsync(draft);

        var key = coordinator.KeyOf(draft)!;
        Assert.True(key.IsNewDocument);
        Assert.NotNull(await _store.LoadAsync(key));

        // The server names the document; the new-document draft is settled and nothing stays under the old id.
        draft.LoadFrom("""{"id":"INV-9","version":1,"title":"brand new"}""");
        coordinator.DraftSettled("invoice", "INV-9");
        await coordinator.FlushAllAsync();
        Assert.Empty(await _store.ListAsync(Models.Scope));
    }

    [Fact]
    public async Task Sign_out_stops_writing_and_removes_everything()
    {
        using var coordinator = Coordinator();
        var draft = Models.LoadedInvoice();
        Wire(coordinator, draft);
        draft.Set("title", new TextFieldValue("x"));
        await coordinator.FlushAsync(draft);

        await coordinator.SignOutAsync();
        draft.Set("title", new TextFieldValue("after sign-out"));
        _clock.Advance(TimeSpan.FromMinutes(1));

        Assert.Empty(await _store.ListAsync(Models.Scope));
        Assert.Single(_store.Saves);
    }
}
