using System.Text.Json.Nodes;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.LogicalTree;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Spotledger.Client.Notifications;
using Spotledger.Client.Notifications.Tests;
using Spotledger.Client.Realtime;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

/// <summary>
/// The forms side of the notification centre (WP-UI-02.T6): the adapter the staleness watcher reads, the
/// banner slot that hosts the bar, one join per tab, and the merge of a dirty form with the server's
/// version in which a field changed on both sides is never overwritten without the person choosing.
/// </summary>
public sealed class StalenessMergeTests
{
    private static string Title(DocumentFormViewModel form) => ((TextFieldValue)form.Draft!.Get("title")!).Value;

    private static string Amount(DocumentFormViewModel form) => ((DecimalFieldValue)form.Draft!.Get("amount")!).CanonicalText;

    private static void ServerChangesTitleAndAmount(FakeGateway gateway)
    {
        gateway.OtherPersonChanges("INV-1", "Theirs");
        gateway.Documents["INV-1"]["amount"] = "50.00";
    }

    [Fact]
    public async Task TheAdapterReportsIdentityVersionAndDirtyStateAndReloadsThroughAPublicMethod()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        var document = new WatchedForm(form);

        Assert.Equal("invoice", document.DocType);
        Assert.Equal("INV-1", document.Id);
        Assert.Equal(1, document.LoadedVersion);
        Assert.False(document.IsDirty);

        form.Draft!.TrySetText("title", "Mine");
        Assert.True(document.IsDirty);

        gateway.OtherPersonChanges("INV-1", "Theirs");
        await document.ReloadAsync();

        Assert.Equal(2, document.LoadedVersion);
        Assert.False(document.IsDirty);
        Assert.Equal("Theirs", Title(form));
    }

    [AvaloniaFact]
    public async Task ACleanFormOffersReloadAndReloadsOnOffer()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        var rooms = new FakeRooms();
        using var staleness = new FormStaleness(form, new DocumentStalenessWatcher(rooms));
        var watch = staleness.Watch!;
        Assert.Equal("doc:invoice:INV-1", watch.Room);

        gateway.OtherPersonChanges("INV-1", "Theirs");
        rooms.Deliver(watch.Room, Events.Doc(RealtimeEventKind.DocUpdated, "invoice", "INV-1", 2));
        Assert.Equal(StalenessKind.ChangedClean, watch.Kind);
        Assert.Same(staleness.Bar, form.Banner);
        Assert.Equal("First invoice", Title(form));

        Assert.True(await watch.ReloadAsync());
        Assert.Equal("Theirs", Title(form));
        Assert.Equal(StalenessKind.None, watch.Kind);
    }

    [AvaloniaFact]
    public async Task ADirtyFormShowsTheBarWithAMergeOfferAndNothingReloadsByItself()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        var rooms = new FakeRooms();
        using var staleness = new FormStaleness(form, new DocumentStalenessWatcher(rooms, () => true));
        form.Draft!.TrySetText("title", "Mine");

        gateway.OtherPersonChanges("INV-1", "Theirs");
        rooms.Deliver(staleness.Watch!.Room, Events.Doc(RealtimeEventKind.DocUpdated, "invoice", "INV-1", 2));
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();

        Assert.Equal(StalenessKind.ChangedDirty, staleness.Watch.Kind);
        Assert.True(staleness.Bar!.IsVisible);
        var buttons = staleness.Bar.GetLogicalDescendants().OfType<Button>().Select(b => b.Content as string).ToList();
        Assert.Contains("Merge my changes", buttons);
        Assert.Contains("Reload (discard my changes)", buttons);
        Assert.Equal("Mine", Title(form));
        Assert.Equal(1, form.Draft.Version);
    }

    [Fact]
    public async Task MergeKeepsMyEditsAppliesTheRestSilentlyAndListsAFieldChangedOnBothSidesAsAConflict()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        form.Draft!.TrySetText("title", "Mine");
        ServerChangesTitleAndAmount(gateway);

        Assert.True(await form.MergeServerChangesAsync());

        var conflict = Assert.Single(form.PendingMerge!.Conflicts);
        Assert.Equal("title", conflict.Field);
        Assert.Equal("Title", conflict.Label);
        Assert.Equal("Mine", conflict.Mine);
        Assert.Equal("Theirs", conflict.Theirs);
        Assert.Equal(MergeSide.Mine, conflict.Side);
        Assert.Equal("Mine", Title(form));
        Assert.Equal("50.00", Amount(form));
        Assert.False(form.Draft.Fields["amount"].IsDirty, "the server's value for an unedited field is applied silently");
        Assert.True(form.Draft.Fields["title"].IsDirty);
        Assert.Equal(2, form.Draft.Version);
        Assert.All(form.Actions, action => Assert.False(action.IsEnabled));
        Assert.DoesNotContain(gateway.Calls, call => call.StartsWith("patch", StringComparison.Ordinal));
    }

    [Fact]
    public async Task AConflictedFieldIsNeverOverwrittenUntilThePersonChoosesTheServersValue()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        form.Draft!.TrySetText("title", "Mine");
        ServerChangesTitleAndAmount(gateway);
        _ = await form.MergeServerChangesAsync();

        Assert.True(form.ApplyMergeChoices());
        Assert.Null(form.PendingMerge);
        Assert.Equal("Mine", Title(form));

        // A second merge, this time taking theirs.
        gateway.OtherPersonChanges("INV-1", "Theirs again");
        Assert.True(await form.MergeServerChangesAsync());
        form.PendingMerge!.Conflicts.Single().Side = MergeSide.Theirs;
        Assert.Equal("Mine", Title(form));
        Assert.True(form.ApplyMergeChoices());
        Assert.Equal("Theirs again", Title(form));
        Assert.False(form.Draft.Fields["title"].IsDirty);
    }

    [Fact]
    public async Task SavingAfterTheMergeSendsTheNewVersionAndOnlyMyChoices()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        form.Draft!.TrySetText("title", "Mine");
        ServerChangesTitleAndAmount(gateway);
        _ = await form.MergeServerChangesAsync();
        Assert.False(await form.RunActionAsync("save"), "the toolbar's save waits for the person's choice");
        Assert.DoesNotContain(gateway.Calls, call => call.StartsWith("patch", StringComparison.Ordinal));
        _ = form.ApplyMergeChoices();

        Assert.True(await form.SaveAsync());

        Assert.Contains("patch INV-1 @2", gateway.Calls);
        Assert.Equal("Mine", gateway.Documents["INV-1"]["title"]!.GetValue<string>());
        Assert.Equal("50.00", gateway.Documents["INV-1"]["amount"]!.GetValue<string>());
        Assert.Equal(3, gateway.Documents["INV-1"]["version"]!.GetValue<int>());
        Assert.DoesNotContain("amount", gateway.Bodies[^1], StringComparison.Ordinal);
    }

    [Fact]
    public async Task WhenNoEditedFieldChangedOnTheServerTheMergeNeedsNoChoice()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        form.Draft!.TrySetText("title", "Mine");
        gateway.Documents["INV-1"]["amount"] = "50.00";
        gateway.Documents["INV-1"]["version"] = 2;

        Assert.True(await form.MergeServerChangesAsync());

        Assert.Null(form.PendingMerge);
        Assert.Equal("Mine", Title(form));
        Assert.Equal("50.00", Amount(form));
        Assert.Equal(2, form.Draft.Version);
        Assert.Contains(form.Actions, action => action.Id == "save" && action.IsEnabled);
    }

    [Fact]
    public async Task AChildTableEditedByMeAndChangedOnTheServerIsAConflictToo()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        form.Draft!.Table("items").AddRow().SetValue("qty", ValueCodec.DecodeDecimalString("7.00"));
        gateway.OtherPersonChanges("INV-1", "Theirs");
        ((JsonArray)gateway.Documents["INV-1"]["items"]!)[0]!["qty"] = "9.00";

        Assert.True(await form.MergeServerChangesAsync());

        var conflict = Assert.Single(form.PendingMerge!.Conflicts);
        Assert.Equal("items", conflict.Field);
        Assert.Equal("3 rows", conflict.Mine);
        Assert.Equal("2 rows", conflict.Theirs);
        Assert.Equal(3, form.Draft.Table("items").Rows.Count);
        conflict.Side = MergeSide.Theirs;
        _ = form.ApplyMergeChoices();
        Assert.Equal(2, form.Draft.Table("items").Rows.Count);
        Assert.Equal("Theirs", Title(form));
    }

    [AvaloniaFact]
    public async Task TheMergeDialogShowsBothValuesAndTheChoiceIsMadeInTheView()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var (view, form, window) = await FormViewTests.ShowAsync(gateway, "INV-1");
        ((SpotTextInput)view.ControlOf("title")!).Text = "Mine";
        ServerChangesTitleAndAmount(gateway);

        Assert.True(await form.MergeServerChangesAsync());
        TestHost.Pump();

        var radios = view.GetVisualDescendants().OfType<RadioButton>().ToList();
        Assert.Equal(2, radios.Count);
        Assert.Equal("Keep mine: Mine", ((TextBlock)radios[0].Content!).Text);
        Assert.Equal("Take theirs: Theirs", ((TextBlock)radios[1].Content!).Text);
        Assert.True(radios[0].IsChecked);

        radios[1].IsChecked = true;
        Assert.Equal(MergeSide.Theirs, form.PendingMerge!.Conflicts[0].Side);
        var apply = view.GetVisualDescendants().OfType<SpotButton>().First(b => b.Name == "MergeApply");
        apply.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));
        TestHost.Pump();

        Assert.Null(form.PendingMerge);
        Assert.Empty(view.GetVisualDescendants().OfType<RadioButton>());
        Assert.Equal("Theirs", Title(form));
        window.Close();
    }

    [AvaloniaFact]
    public async Task ThereIsOneJoinPerOpenTabAndANewDocumentJoinsWhenItIsFirstSaved()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var rooms = new FakeRooms();
        var watcher = new DocumentStalenessWatcher(rooms);
        var opened = await Fixtures.OpenAsync(gateway, "INV-1");
        var created = Fixtures.Form(gateway);
        await created.LoadAsync();

        var first = new FormStaleness(opened, watcher);
        var second = new FormStaleness(created, watcher);
        Assert.Equal(1, watcher.ActiveWatches);
        Assert.Null(second.Watch);

        created.Draft!.TrySetText("title", "Brand new");
        Assert.True(await created.SaveAsync());
        Assert.Equal(2, watcher.ActiveWatches);
        Assert.Equal(2, rooms.Active);
        Assert.Equal($"doc:invoice:{created.DocumentId}", second.Watch!.Room);

        first.Dispose();
        Assert.Equal(1, rooms.Active);
        second.Dispose();
        Assert.Equal(0, rooms.Active);
        Assert.Equal(0, watcher.ActiveWatches);
        Assert.Null(opened.Banner);
    }
}
