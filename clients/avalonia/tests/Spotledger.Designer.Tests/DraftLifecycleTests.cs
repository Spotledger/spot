using Spotledger.Client.Designer;
using Xunit;

namespace Spotledger.Designer.Tests;

/// <summary>
/// The draft lifecycle: autosave after every accepted op, undo and redo over the server's op-log
/// cursor, the history list, discard, and publish gated by the server's permission rather than by
/// the button's enabled state.
/// </summary>
public sealed class DraftLifecycleTests
{
    [Fact]
    public async Task UndoAndRedoMoveTheServerOpLogCursor()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddSectionAsync("Request", "request", 2);
        await harness.AddFieldAsync("Request Date", "date", "request");
        await harness.AddFieldAsync("Customer Name", "text", "request", "request_date");
        Assert.Equal(2, harness.Server.Current.Section("request", null)!.Items.Count);
        Assert.True(harness.Designer.CanUndo);
        Assert.False(harness.Designer.CanRedo);

        await DesignerHarness.RunAsync(harness.Designer.UndoCommand);
        Assert.True(harness.Designer.CanUndo);
        Assert.True(harness.Designer.CanRedo);
        Assert.Single(harness.Server.Current.Section("request", null)!.Items);

        await DesignerHarness.RunAsync(harness.Designer.RedoCommand);
        Assert.Equal(2, harness.Server.Current.Section("request", null)!.Items.Count);
        Assert.True(harness.Designer.CanUndo);
        Assert.False(harness.Designer.CanRedo);

        // The history list is the server's log with the cursor marked, not a client stack.
        Assert.Equal(harness.Session.Sent.Count, harness.Designer.History.Count);
        Assert.Single(harness.Designer.History, entry => entry.IsCurrent);
    }

    [Fact]
    public async Task EveryAcceptedOpIsAutosaved()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddSectionAsync("Request", "request", 2);
        await harness.AddFieldAsync("Request Date", "date", "request");

        Assert.Equal(SaveState.Saved, harness.Session.SaveState);
        Assert.Equal("Saved", harness.Designer.SaveStateText);
        Assert.Equal(harness.Session.Sent.Count, harness.Server.Saves.Count - 1); // the open saved once
        Assert.All(harness.Server.Saves, version => Assert.True(version > 0));

        // Saving again is harmless and adds no op.
        var ops = harness.IssuedOps.Count;
        await DesignerHarness.RunAsync(harness.Designer.SaveCommand);
        Assert.Equal(ops, harness.IssuedOps.Count);
    }

    [Fact]
    public async Task DiscardClosesTheDraftAndItCanNeverPublish()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddSectionAsync("Request", "request", 2);
        await harness.AddFieldAsync("Request Date", "date", "request");

        await DesignerHarness.RunAsync(harness.Designer.DiscardCommand);

        Assert.Equal(1, harness.Server.DiscardCount);
        Assert.Equal("discarded", harness.Server.Current.Status);
        Assert.Equal("discarded", harness.Designer.Session.Draft!.Status);

        await harness.Designer.Publish.OpenAsync();
        Assert.False(harness.Designer.Publish.Plan!.Ready);
        harness.Designer.Publish.IsConfirmed = true;
        await harness.Designer.Publish.ConfirmAsync();
        Assert.Equal(0, harness.Server.PublishCount);

        // The server refuses a publish of a discarded draft even when the plan was built anyway.
        var refusal = await harness.Session.PublishAsync();
        Assert.False(refusal.Ok);
        Assert.Equal(0, harness.Server.PublishCount);
    }

    [Fact]
    public async Task PublishIsGatedByTheServerPermission()
    {
        var harness = await DesignerHarness.Create(canPublish: false).OpenAsync();
        await harness.AddSectionAsync("Request", "request", 2);
        await harness.AddFieldAsync("Title", "text", "request");

        Assert.False(harness.Designer.CanPublish);
        await harness.Designer.Publish.OpenAsync();
        Assert.True(harness.Designer.Publish.Plan!.Ready);
        Assert.Contains("permission", harness.Designer.Publish.BlockedReason, StringComparison.OrdinalIgnoreCase);

        harness.Designer.Publish.IsConfirmed = true;
        Assert.False(harness.Designer.Publish.CanConfirm);
        await harness.Designer.Publish.ConfirmAsync();
        Assert.Equal(0, harness.Server.PublishCount);

        // The session's own publish path reports the server's refusal rather than guessing.
        var refused = await harness.Session.PublishAsync();
        Assert.False(refused.Ok);
        Assert.Equal("nocode_model.publish_forbidden", refused.Code);

        // With the permission, the same dialog publishes.
        harness.Server.CanPublish = true;
        await harness.Session.RefreshAsync();
        harness.Designer.Refresh();
        Assert.True(harness.Designer.CanPublish);

        await harness.Designer.Publish.OpenAsync();
        harness.Designer.Publish.IsConfirmed = true;
        Assert.True(harness.Designer.Publish.CanConfirm);
        await harness.Designer.Publish.ConfirmAsync();

        Assert.Equal(1, harness.Server.PublishCount);
        Assert.Equal("published", harness.Server.Current.Status);
        Assert.Contains("published as version", harness.Designer.Publish.Result, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public async Task TypeChangeIsClassifiedBeforeAnythingIsApplied()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddFieldAsync("Priority", "select");
        await harness.SetOptionsAsync("priority", "Low", "High");
        harness.Designer.SelectField("priority");

        // A child table is not a field type: the server refuses it and nothing is sent.
        harness.Designer.Inspector.SelectedTypeChoice = FieldTypeCatalog.Find("table");
        await DesignerHarness.RunAsync(harness.Designer.Inspector.PlanSelectedTypeChangeCommand);
        var refused = harness.Designer.Inspector.TypeChange!;
        Assert.Equal(TypeChangeClassification.Refused, refused.Classification);
        Assert.Equal("Not allowed", refused.ClassificationText);
        Assert.StartsWith("Not allowed", harness.Designer.Inspector.TypeChangeText, StringComparison.Ordinal);

        var ops = harness.IssuedOps.Count;
        await DesignerHarness.RunAsync(harness.Designer.Inspector.ApplyTypeChangeCommand);
        Assert.Equal(ops, harness.IssuedOps.Count);
        Assert.Equal("select", harness.Server.Current.Field("priority", null)!.Type);

        // Changing a choice list to text loses nothing, and that change is applied.
        harness.Designer.Inspector.SelectedTypeChoice = FieldTypeCatalog.Find("text");
        await DesignerHarness.RunAsync(harness.Designer.Inspector.PlanSelectedTypeChangeCommand);
        Assert.Equal(TypeChangeClassification.Safe, harness.Designer.Inspector.TypeChange!.Classification);
        await DesignerHarness.RunAsync(harness.Designer.Inspector.ApplyTypeChangeCommand);

        Assert.Equal("text", harness.Server.Current.Field("priority", null)!.Type);
        Assert.Equal("change_type|priority|text|-", harness.IssuedOps[^1]);
    }

    [Fact]
    public async Task LossyTypeChangeSaysWhatHappensToTheValues()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddFieldAsync("Notes", "text");
        harness.Designer.SelectField("notes");

        harness.Designer.Inspector.SelectedTypeChoice = FieldTypeCatalog.Find("int");
        await DesignerHarness.RunAsync(harness.Designer.Inspector.PlanSelectedTypeChangeCommand);

        var plan = harness.Designer.Inspector.TypeChange!;
        Assert.Equal(TypeChangeClassification.Lossy, plan.Classification);
        Assert.Contains("not numbers", plan.Summary, StringComparison.OrdinalIgnoreCase);
        Assert.NotEmpty(plan.DetailList);
        Assert.True(plan.CanApply);
    }
}
