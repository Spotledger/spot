using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

public sealed class RecoveryTests
{
    private readonly ManualClock _clock = new();
    private readonly InMemoryDraftStore _store = new();
    private static readonly DraftKey InvoiceKey = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");

    private async Task<DraftRecord> StoreEditedAsync(Action<DocumentDraft>? edit = null)
    {
        var draft = Models.LoadedInvoice();
        draft.Set("title", new TextFieldValue("My edit"));
        draft.Tables["items"].Rows[0].SetValue("qty", new DecimalFieldValue(7m));
        draft.Tables["items"].AddRow().SetValue("item", new TextFieldValue("Washer"));
        edit?.Invoke(draft);
        var record = DraftSnapshot.Capture(draft, InvoiceKey, _clock.GetUtcNow(), "October", "tab=1");
        _ = await _store.SaveAsync(record);
        return record;
    }

    [Fact]
    public async Task Opening_a_document_with_a_draft_offers_a_restore_bar_with_the_time()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice();

        var bar = await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", draft, formatTime: t => t.ToString("HH:mm", System.Globalization.CultureInfo.InvariantCulture));

        Assert.NotNull(bar);
        Assert.True(bar.IsVisible);
        Assert.Equal("Restore unsaved changes from 09:00", bar.Message);
        Assert.Equal("Restore", bar.RestoreLabel);
        Assert.Equal("Discard", bar.DiscardLabel);
        Assert.False(draft.IsDirty);
    }

    [Fact]
    public async Task No_draft_means_no_bar()
    {
        Assert.Null(await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", Models.LoadedInvoice()));
        Assert.Null(await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", new DocumentDraft(Models.Invoice())));
    }

    [Fact]
    public async Task Restore_against_an_unchanged_server_version_applies_the_edits_as_unsaved_changes()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice();
        var bar = (await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", draft))!;
        Assert.False(bar.Plan.NeedsReview);

        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;

        Assert.False(bar.IsVisible);
        Assert.Equal("My edit", ((TextFieldValue)draft.Get("title")!).Value);
        Assert.True(draft.IsDirty);
        var rows = draft.Tables["items"].Rows;
        Assert.Equal(3, rows.Count);
        Assert.Equal("r1", rows[0].Id);
        Assert.Equal("7", ((DecimalFieldValue)rows[0].GetValue("qty")!).CanonicalText.TrimEnd('0').TrimEnd('.'));
        Assert.True(rows[2].IsNew);
        Assert.Equal("Restored 2 changes.", bar.Result);
    }

    [Fact]
    public async Task Restore_against_a_changed_server_version_shows_what_changed_and_never_overwrites_silently()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice(version: 4, title: "Edited on the server");
        var bar = (await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", draft))!;

        Assert.True(bar.Plan.NeedsReview);
        Assert.Equal("Review", bar.RestoreLabel);
        var change = Assert.Single(bar.ServerChanges);
        Assert.Equal("Title", change.Label);
        Assert.Equal("My edit", change.YourValue);
        Assert.Equal("Edited on the server", change.ServerValue);

        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;
        Assert.Equal(RecoveryBarState.Reviewing, bar.State);
        Assert.True(bar.IsReviewing);
        Assert.False(draft.IsDirty);

        bar.ApplyUntouchedCommand.Execute(null);
        await bar.ApplyUntouchedCommand.ExecutionTask;
        Assert.Equal("Edited on the server", ((TextFieldValue)draft.Get("title")!).Value);
        Assert.Equal(3, draft.Tables["items"].Rows.Count);
        Assert.False(bar.IsVisible);
    }

    [Fact]
    public async Task The_user_can_choose_their_own_values_over_the_servers_changes()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice(version: 4, title: "Edited on the server");
        var bar = (await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", draft))!;
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;

        bar.ApplyAllCommand.Execute(null);
        await bar.ApplyAllCommand.ExecutionTask;

        Assert.Equal("My edit", ((TextFieldValue)draft.Get("title")!).Value);
        Assert.True(draft.IsDirty);
    }

    [Fact]
    public async Task A_newer_version_that_touched_other_fields_restores_without_a_review_step()
    {
        _ = await StoreEditedAsync();
        var draft = new DocumentDraft(Models.Invoice());
        draft.LoadFrom("""{"id":"INV-1","version":9,"title":"October","qty":"1.00","amount":"55.00","ssn":"123-45","items":[{"id":"r1","idx":1,"item":"Bolt","qty":"2.00"},{"id":"r2","idx":2,"item":"Nut","qty":"3.00"}]}""");
        var bar = (await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", draft))!;

        Assert.True(bar.Plan.ServerVersionChanged);
        Assert.Empty(bar.ServerChanges);
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;
        Assert.Equal("My edit", ((TextFieldValue)draft.Get("title")!).Value);
    }

    [Fact]
    public async Task A_child_table_that_changed_on_the_server_is_a_conflict()
    {
        _ = await StoreEditedAsync();
        var draft = new DocumentDraft(Models.Invoice());
        draft.LoadFrom("""{"id":"INV-1","version":4,"title":"October","qty":"1.00","amount":"10.00","ssn":"123-45","items":[{"id":"r1","idx":1,"item":"Bolt","qty":"2.00"}]}""");
        var bar = (await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", draft))!;

        var change = Assert.Single(bar.ServerChanges);
        Assert.Equal("Items", change.Label);
        Assert.Equal(RestoreItemKind.Table, change.Kind);
    }

    [Fact]
    public async Task A_field_the_user_can_no_longer_see_is_skipped_with_a_reason_not_written_back()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice();
        Models.Mask(draft, "title");
        var bar = new RecoveryBarViewModel(_store, (await _store.LoadAsync(InvoiceKey))!, draft);

        var skipped = Assert.Single(bar.Skipped);
        Assert.Equal("title", skipped.Name);
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;
        Assert.True(draft.Fields["title"].IsMasked);
        Assert.Equal(3, draft.Tables["items"].Rows.Count);
    }

    [Fact]
    public async Task Restore_cannot_bypass_the_edit_gate_on_a_read_only_document()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice();
        var bar = new RecoveryBarViewModel(_store, (await _store.LoadAsync(InvoiceKey))!, draft, isDocumentReadOnly: () => true);

        Assert.All(bar.Plan.Items, i => Assert.NotNull(i.SkipReason));
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;

        Assert.False(draft.IsDirty);
    }

    [Fact]
    public async Task Restore_skips_a_field_that_is_no_longer_editable()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice();
        draft.ApplyPermissions(PermissionSnapshot.Decode(
            """{"etag":"e1","data":{"doctypes":[{"doctype":"invoice","rights":{"create":true,"write":true},"fields":{"read_only":["title"]}}]}}"""));
        var bar = new RecoveryBarViewModel(_store, (await _store.LoadAsync(InvoiceKey))!, draft);

        Assert.Contains(bar.Skipped, i => i.Name == "title");
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;
        Assert.NotEqual("My edit", (draft.Fields["title"].Value as TextFieldValue)?.Value);
    }

    [Fact]
    public async Task Discard_removes_the_stored_draft()
    {
        _ = await StoreEditedAsync();
        var draft = Models.LoadedInvoice();
        var bar = (await RecoveryBarViewModel.TryCreateAsync(_store, Models.Scope, "invoice", draft))!;

        bar.DiscardCommand.Execute(null);
        await bar.DiscardCommand.ExecutionTask;

        Assert.False(bar.IsVisible);
        Assert.Null(await _store.LoadAsync(InvoiceKey));
        Assert.False(draft.IsDirty);
    }

    [Fact]
    public async Task The_recover_list_shows_every_draft_newest_first_for_this_user_and_server_only()
    {
        _ = await StoreEditedAsync();
        _clock.Advance(TimeSpan.FromHours(1));
        var fresh = new DocumentDraft(Models.Invoice());
        fresh.Set("title", new TextFieldValue("unsaved new"));
        var newKey = DraftKey.ForNew(Models.Scope, "invoice");
        _ = await _store.SaveAsync(DraftSnapshot.Capture(fresh, newKey, _clock.GetUtcNow(), "unsaved new"));
        var other = DraftKey.ForDocument(new DraftScope(Models.Scope.ServerId, "bob"), "invoice", "INV-1");
        _ = await _store.SaveAsync(DraftSnapshot.Capture(Models.LoadedInvoice(), other, _clock.GetUtcNow()));
        var edited = Models.LoadedInvoice();
        edited.Set("title", new TextFieldValue("bob's"));
        _ = await _store.SaveAsync(DraftSnapshot.Capture(edited, other, _clock.GetUtcNow()));

        var opened = new List<DraftKey>();
        var list = new RecoverDraftsViewModel(_store, Models.Scope, key => { opened.Add(key); return Task.CompletedTask; }, name => name == "invoice" ? "Invoice" : name, t => t.ToString("HH:mm", System.Globalization.CultureInfo.InvariantCulture));
        await list.LoadAsync();

        Assert.True(list.HasDrafts);
        Assert.Equal(["New Invoice - unsaved new", "Invoice INV-1 - October"], list.Items.Select(i => i.Document));
        Assert.Equal(["10:00", "09:00"], list.Items.Select(i => i.TimeText));

        list.OpenCommand.Execute(list.Items[1]);
        await list.OpenCommand.ExecutionTask;
        Assert.Equal(InvoiceKey, Assert.Single(opened));

        list.DiscardCommand.Execute(list.Items[0]);
        await list.DiscardCommand.ExecutionTask;
        Assert.Single(list.Items);
        Assert.Null(await _store.LoadAsync(newKey));
        Assert.NotNull(await _store.LoadAsync(other));

        list.DiscardAllCommand.Execute(null);
        await list.DiscardAllCommand.ExecutionTask;
        Assert.False(list.HasDrafts);
    }

    [Fact]
    public async Task A_new_document_draft_is_recoverable_into_a_new_form()
    {
        var fresh = new DocumentDraft(Models.Invoice());
        fresh.Set("title", new TextFieldValue("unsaved new"));
        fresh.Tables["items"].AddRow().SetValue("item", new TextFieldValue("Washer"));
        var key = DraftKey.ForNew(Models.Scope, "invoice");
        _ = await _store.SaveAsync(DraftSnapshot.Capture(fresh, key, _clock.GetUtcNow()));

        var target = new DocumentDraft(Models.Invoice());
        var bar = new RecoveryBarViewModel(_store, (await _store.LoadAsync(key))!, target);
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;

        Assert.Equal("unsaved new", ((TextFieldValue)target.Get("title")!).Value);
        Assert.Single(target.Tables["items"].Rows);
    }
}
