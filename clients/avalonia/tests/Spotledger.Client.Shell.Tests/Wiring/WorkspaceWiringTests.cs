using Avalonia.Headless.XUnit;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;
using Spotledger.Client.Workspace;
using Xunit;

namespace Spotledger.Client.Shell.Tests.Wiring;

/// <summary>Drafts and undo wired into the desktop host (WP-UI-17.T4): what is saved, what is offered, what is wiped, and that nothing is sent.</summary>
public sealed class WorkspaceWiringTests
{
    private static readonly TimeSpan Debounce = TimeSpan.FromSeconds(3);

    private static async Task<byte[]> EditAndWaitForDraftAsync(Rig rig, DocumentFormViewModel form, string title = "Unsaved title ZZQ")
    {
        form.Draft!.Set("title", new TextFieldValue(title));
        rig.Clock.Advance(Debounce);
        await TestHarness.WaitUntilAsync(() => rig.DraftFiles.Any(), "a draft file");
        return File.ReadAllBytes(rig.DraftFiles.First());
    }

    private static async Task CloseFormAsync(Rig rig)
    {
        rig.Shell.CloseTab(rig.Shell.Tabs.Single(t => t.Content is DocumentFormViewModel));
        await TestHarness.WaitUntilAsync(() => rig.Shell.Tabs.All(t => t.Content is not DocumentFormViewModel), "the tab to close");
        await Task.Delay(50);
    }

    [AvaloniaFact]
    public async Task EditedFormSavesAnEncryptedDraftAndSendsNothing()
    {
        using var rig = new Rig(new WiringGateway { MaskedField = "secret_note" });
        await rig.SignInAsync();
        var form = await rig.OpenAsync();

        var bytes = await EditAndWaitForDraftAsync(rig, form);

        var text = System.Text.Encoding.UTF8.GetString(bytes);
        Assert.DoesNotContain("ZZQ", text, StringComparison.Ordinal);
        Assert.DoesNotContain("plain-secret-note", text, StringComparison.Ordinal);
        Assert.False(rig.Gateway.SentWrite, "autosave never sends");
        Assert.Contains(rig.Secrets.Secrets.Keys, key => key.StartsWith("spot-drafts/", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public async Task ReopenedFormOffersTheDraftAndRestoreAndDiscardSendNothing()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        _ = await EditAndWaitForDraftAsync(rig, form);
        await CloseFormAsync(rig);

        var again = await rig.OpenAsync();
        var bar = Assert.IsType<RecoveryBarView>(again.Recovery).DataContext as RecoveryBarViewModel;
        Assert.NotNull(bar);
        Assert.Equal("October", ((TextFieldValue)again.Draft!.Get("title")!).Value);

        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;
        Assert.Equal("Unsaved title ZZQ", ((TextFieldValue)again.Draft.Get("title")!).Value);
        Assert.False(rig.Gateway.SentWrite);

        // Discard on a second opening deletes the draft.
        await CloseFormAsync(rig);
        var third = await rig.OpenAsync();
        var second = (RecoveryBarViewModel)((RecoveryBarView)third.Recovery!).DataContext!;
        second.DiscardCommand.Execute(null);
        await second.DiscardCommand.ExecutionTask;
        await TestHarness.WaitUntilAsync(() => !rig.DraftFiles.Any(), "the draft to be deleted");
        Assert.False(rig.Gateway.SentWrite);
    }

    [AvaloniaFact]
    public async Task SignOutRemovesDraftsAndKeyAndAnInFlightWriteDoesNotComeBack()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        _ = await EditAndWaitForDraftAsync(rig, form);
        form.Draft!.Set("title", new TextFieldValue("Another edit")); // a write is pending

        rig.Shell.LogoutCommand.Execute(null);
        await rig.Shell.LogoutCommand.ExecutionTask;
        await TestHarness.WaitUntilAsync(() => !rig.DraftFiles.Any() && rig.Secrets.Secrets.Count == 0, "drafts and key to be wiped");

        rig.Clock.Advance(TimeSpan.FromSeconds(30));
        await Task.Delay(100);
        Assert.Empty(rig.DraftFiles);
        Assert.Empty(rig.Secrets.Secrets);
        Assert.Null(rig.Drafts.Scope);
        Assert.Empty(rig.Shell.Tabs);
    }

    [AvaloniaFact]
    public async Task ASwitchOfUserOrServerNeverCrossesDrafts()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var scopeA = rig.Drafts.Scope!;
        var form = await rig.OpenAsync();
        _ = await EditAndWaitForDraftAsync(rig, form);
        var coordinatorA = rig.Drafts.Coordinator!;

        var userB = new DraftScope(scopeA.ServerId, "another-user");
        await rig.Drafts.SwitchAsync(userB);
        Assert.Same(userB, rig.Drafts.Scope);
        Assert.NotSame(coordinatorA, rig.Drafts.Coordinator);
        Assert.Equal(userB, rig.Drafts.Coordinator!.Scope);
        Assert.Empty(await rig.Drafts.Store!.ListAsync(userB, TestContext.Current.CancellationToken));
        Assert.True(rig.DraftFiles.Any(), "user A's draft stays on disk for A's next sign-in");

        var otherServer = new DraftScope("https://other.example.test", scopeA.UserId);
        await rig.Drafts.SwitchAsync(otherServer);
        Assert.Empty(await rig.Drafts.Store!.ListAsync(otherServer, TestContext.Current.CancellationToken));

        // The old coordinator was disposed: a late edit of A's form writes nothing for anyone.
        var before = rig.DraftFiles.Count();
        coordinatorA.DraftChanged("invoice", "INV-1", form.Draft!);
        rig.Clock.Advance(Debounce);
        await Task.Delay(100);
        Assert.Equal(before, rig.DraftFiles.Count());
    }

    [AvaloniaFact]
    public async Task RestoreIsRefusedIntoAPostedDocument()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        _ = await EditAndWaitForDraftAsync(rig, form);
        await CloseFormAsync(rig);

        rig.Gateway.State = "posted";
        var again = await rig.OpenAsync();
        var bar = (RecoveryBarViewModel)((RecoveryBarView)again.Recovery!).DataContext!;
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;

        Assert.Equal("October", ((TextFieldValue)again.Draft!.Get("title")!).Value);
        Assert.Contains("read-only", bar.Result, StringComparison.Ordinal);
        Assert.False(rig.Gateway.SentWrite);
    }

    [AvaloniaFact]
    public async Task RestoreIsRefusedWhenTheUserLostTheWriteRight()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        _ = await EditAndWaitForDraftAsync(rig, form);
        await CloseFormAsync(rig);

        rig.Gateway.Rights = rig.Gateway.Rights with { Write = false };
        var again = await rig.OpenAsync();
        var bar = (RecoveryBarViewModel)((RecoveryBarView)again.Recovery!).DataContext!;
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;

        Assert.Equal("October", ((TextFieldValue)again.Draft!.Get("title")!).Value);
        Assert.Contains("read-only", bar.Result, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public async Task RestoreIsRefusedIntoAFieldThatIsMaskedNow()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        form.Draft!.Set("secret_note", new TextFieldValue("note edited while visible"));
        form.Draft.Set("title", new TextFieldValue("Title edited too"));
        rig.Clock.Advance(Debounce);
        await TestHarness.WaitUntilAsync(() => rig.DraftFiles.Any(), "a draft file");
        await CloseFormAsync(rig);

        rig.Gateway.MaskedField = "secret_note";
        var again = await rig.OpenAsync();
        var bar = (RecoveryBarViewModel)((RecoveryBarView)again.Recovery!).DataContext!;
        Assert.Contains(bar.Skipped, item => item.Name == "secret_note" && item.SkipReason is not null);

        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;
        Assert.Equal("Title edited too", ((TextFieldValue)again.Draft!.Get("title")!).Value);
        Assert.True(again.Draft.Fields["secret_note"].IsMasked);
        Assert.Equal(FieldValue.Null, again.Draft.Fields["secret_note"].Value);
    }

    [AvaloniaFact]
    public async Task FormAndChildTableShareOneHistoryAndAServerDiffUndoesWithItsCause()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        using var view = new DocumentFormView(form);
        var history = form.History!;
        var table = form.Draft!.Tables["items"];

        form.Draft.Set("title", new TextFieldValue("Changed"));
        Assert.True(history.CanUndo);
        form.ApplyServerDiff(draft => draft.Set("total", new DecimalFieldValue(99.5m)));
        _ = table.AddRow();
        table.Rows[^1].SetValue("item", new TextFieldValue("Washer"));

        // The server-derived total is part of the title edit, not a step of its own.
        Assert.Equal("Edit Title", RigLabel(history, undo: true, steps: 2));

        Assert.True(history.Undo().Succeeded); // the row
        Assert.Equal(2, table.Rows.Count);
        Assert.True(history.Undo().Succeeded); // the title and the total derived from it
        Assert.Equal("October", ((TextFieldValue)form.Draft.Get("title")!).Value);
        Assert.Equal(10.00m, ((DecimalFieldValue)form.Draft.Get("total")!).ExactValue.AsDecimal);
        Assert.True(history.Redo().Succeeded);
        Assert.Equal(99.5m, ((DecimalFieldValue)form.Draft.Get("total")!).ExactValue.AsDecimal);
        Assert.False(rig.Gateway.SentWrite);
    }

    private static string? RigLabel(IFormHistory history, bool undo, int steps)
    {
        // The label of the step below the top one: undo the top steps, read, replay them.
        string? label = null;
        for (var i = 0; i < steps - 1; i++)
        {
            _ = history.Undo();
        }

        label = undo ? history.UndoLabel : history.RedoLabel;
        for (var i = 0; i < steps - 1; i++)
        {
            _ = history.Redo();
        }

        return label;
    }

    [AvaloniaFact]
    public async Task UndoCannotBypassTheGate()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        form.Draft!.Set("title", new TextFieldValue("Changed"));
        Assert.True(form.History!.CanUndo);

        // The permission summary takes the title away from this user: the undo of the earlier edit is refused.
        form.Draft.ApplyPermissions(PermissionSnapshot.Decode(
            """{"etag":"e2","data":{"doctypes":[{"doctype":"invoice","rights":{"create":true,"write":true},"fields":{"read_only":["title"]}}]}}"""));
        var refused = form.History.Undo();
        Assert.False(refused.Succeeded);
        Assert.Equal("Changed", ((TextFieldValue)form.Draft.Get("title")!).Value);
        Assert.True(form.History.CanUndo, "a refusal leaves the step in place");
        await rig.Shell.RunAsync(WorkspaceCommands.UndoId, CommandRoute.Shortcut);
        Assert.Contains("cannot change", rig.Shell.StatusMessage, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public async Task ADerivedReadOnlyFieldDoesNotRefuseTheUndoOfItsCause()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        Assert.True(form.Draft!.Fields["total"].IsReadOnly);
        Assert.Null(form.CheckFieldForHistory("total"));
    }

    [AvaloniaFact]
    public async Task RecoverDraftsCommandOpensTheListAndOpeningADraftOpensTheDocumentWithTheBar()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        _ = await EditAndWaitForDraftAsync(rig, form);
        await CloseFormAsync(rig);
        await rig.Host.RefreshCountAsync();
        Assert.Equal(1, rig.Host.DraftCount);
        Assert.Equal("Recover drafts (1)", rig.Shell.Commands[WorkspaceCommands.RecoverDraftsId].Label);

        var result = await rig.Shell.RunAsync(WorkspaceCommands.RecoverDraftsId, CommandRoute.Menu);
        Assert.True(result.Executed);
        var list = Assert.IsType<RecoverDraftsViewModel>(rig.Shell.SelectedTab!.Content);
        var item = Assert.Single(list.Items);

        list.OpenCommand.Execute(item);
        await list.OpenCommand.ExecutionTask;
        var opened = (DocumentFormViewModel)rig.Shell.SelectedTab!.Content!;
        await TestHarness.WaitUntilAsync(() => opened.Phase == FormPhase.Ready && opened.Recovery is not null, "the document and its bar");
        Assert.Equal("INV-1", opened.RequestedId);
        Assert.False(rig.Gateway.SentWrite);
    }

    [AvaloniaFact]
    public async Task RecoverDraftsIsUnavailableWhenSignedOut()
    {
        using var rig = new Rig();
        var answer = rig.Shell.Dispatcher.GetAvailability(WorkspaceCommands.RecoverDraftsId, rig.Shell.CurrentContext());
        Assert.False(answer.IsAvailable);
        await rig.SignInAsync();
        Assert.True(rig.Shell.Dispatcher.GetAvailability(WorkspaceCommands.RecoverDraftsId, rig.Shell.CurrentContext()).IsAvailable);
    }

    [AvaloniaFact]
    public async Task EditMenuLabelsAndAvailabilityFollowTheFocusedForm()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        Assert.Equal("Undo", rig.Shell.Commands[WorkspaceCommands.UndoId].Label);
        Assert.False(rig.Shell.Dispatcher.GetAvailability(WorkspaceCommands.UndoId, rig.Shell.CurrentContext()).IsAvailable);

        var form = await rig.OpenAsync();
        form.Draft!.Set("qty", new DecimalFieldValue(5m));
        Assert.Equal("Undo Edit Quantity", rig.Shell.Commands[WorkspaceCommands.UndoId].Label);
        Assert.True(rig.Shell.Dispatcher.GetAvailability(WorkspaceCommands.UndoId, rig.Shell.CurrentContext()).IsAvailable);
        Assert.False(rig.Shell.Dispatcher.GetAvailability(WorkspaceCommands.RedoId, rig.Shell.CurrentContext()).IsAvailable);

        rig.Shell.IsFocusedText = true;
        Assert.False(rig.Shell.Dispatcher.GetAvailability(WorkspaceCommands.UndoId, rig.Shell.CurrentContext()).IsAvailable);
        rig.Shell.IsFocusedText = false;

        var run = await rig.Shell.RunAsync(WorkspaceCommands.UndoId, CommandRoute.Menu);
        Assert.True(run.Executed);
        Assert.Equal(1.00m, ((DecimalFieldValue)form.Draft.Get("qty")!).ExactValue.AsDecimal);
        Assert.Equal("Redo Edit Quantity", rig.Shell.Commands[WorkspaceCommands.RedoId].Label);
    }

    [AvaloniaFact]
    public async Task EveryKeymapProfileCarriesTheThreeChordsWithoutCollisions()
    {
        using var rig = new Rig();
        foreach (var name in new[] { "desktop", "browser" })
        {
            rig.Shell.SetKeymapProfile(name);
            var chords = rig.Shell.Keymap.Bindings.ToDictionary(b => b.Chord, b => b.CommandId, StringComparer.OrdinalIgnoreCase);
            Assert.Equal(WorkspaceCommands.UndoId, chords["Ctrl+Z"]);
            Assert.Equal(WorkspaceCommands.RedoId, chords["Ctrl+Y"]);
            Assert.Equal(WorkspaceCommands.RedoId, chords["Ctrl+Shift+Z"]);
            rig.Shell.Keymap.Validate(rig.Shell.Registry);
        }

        await Task.CompletedTask;
    }
}
