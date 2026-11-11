using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

/// <summary>The list screen driven through real key events: the keyboard table of the brief.</summary>
public sealed class ListViewTests
{
    [AvaloniaFact]
    public async Task ShowsTheGeneratedColumnsAndTheFirstRows()
    {
        var s = await ViewHarness.ShowAsync();

        Assert.Equal(["Title", "Customer", "Amount", "Status", "Salary"], s.View.Grid.Columns!.Select(c => c.Header));
        Assert.Equal("Order 0", ViewHarness.CellText(s.View.Grid, 0, 0));
        Assert.Equal("Party p-1", ViewHarness.CellText(s.View.Grid, 1, 1));
        Assert.Contains("rows", s.View.StatusLine, StringComparison.Ordinal);
        Assert.Contains("No filter", s.View.StatusLine, StringComparison.Ordinal);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task EnterOpensTheRowInAFormTab()
    {
        var s = await ViewHarness.ShowAsync();
        s.View.Grid.SetCursor(3, 0);

        ViewHarness.Press(s.View.Grid, Key.Enter);

        Assert.Equal([("order", (string?)"d-000003")], s.Shell.Opened);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task CtrlNCreatesAndNeedsTheRight()
    {
        var s = await ViewHarness.ShowAsync();
        ViewHarness.Press(s.View.Grid, Key.N, KeyModifiers.Control);
        Assert.Equal([("order", (string?)null)], s.Shell.Opened);
        s.Window.Close();

        var denied = await ViewHarness.ShowAsync(new FakeListGateway(100) { CanCreate = false });
        ViewHarness.Press(denied.View.Grid, Key.N, KeyModifiers.Control);
        Assert.Empty(denied.Shell.Opened);
        Assert.False(denied.View.Toolbar.Children.OfType<SpotButton>().First(b => (string?)b.Content == "New").IsEnabled);
        denied.Window.Close();
    }

    [AvaloniaFact]
    public async Task F5RefreshesTheQuery()
    {
        var s = await ViewHarness.ShowAsync();
        var before = s.Gateway.Requests.Count;

        ViewHarness.Press(s.View.Grid, Key.F5);

        Assert.True(await ViewHarness.WaitAsync(() => s.Gateway.Requests.Count > before));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task DeleteKeyAsksBeforeDeletingTheSelection()
    {
        var s = await ViewHarness.ShowAsync();
        s.View.Grid.SetSelection([1, 2]);
        TestHost.Pump();
        Assert.Equal(2, s.Model.SelectedIds.Count);

        ViewHarness.Press(s.View.Grid, Key.Delete);
        Assert.True(await ViewHarness.WaitAsync(() => s.Gateway.Deleted.Count == 2));

        Assert.Contains(s.Shell.Asked, a => a.StartsWith("confirm: Delete | Delete 2 documents", StringComparison.Ordinal));
        Assert.Equal(["d-000001", "d-000002"], s.Gateway.Deleted.Select(d => d.Id));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task DeleteIsDisabledWithoutTheDeleteRight()
    {
        var s = await ViewHarness.ShowAsync(new FakeListGateway(100, new Forms.DocumentRights(true, true, true, false, false, false, false, false)));
        s.View.Grid.SetSelection([1]);
        TestHost.Pump();

        ViewHarness.Press(s.View.Grid, Key.Delete);
        await ViewHarness.PumpAsync();

        Assert.False(s.View.Toolbar.Children.OfType<SpotButton>().First(b => (string?)b.Content == "Delete").IsEnabled);
        Assert.Empty(s.Gateway.Deleted);
        Assert.DoesNotContain(s.Shell.Asked, a => a.StartsWith("confirm", StringComparison.Ordinal));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ChipsAreRemovedFromTheKeyboard()
    {
        var s = await ViewHarness.ShowAsync();
        s.Model.AddCondition(new FilterCondition("status", FilterOperator.OneOf, ["Open"]));
        s.Model.AddCondition(new FilterCondition("amount", FilterOperator.From, ["5"]));
        TestHost.Pump();
        var chips = s.View.Chips.Children.OfType<SpotButton>().ToList();
        Assert.Equal(3, chips.Count); // two chips and "Clear all"
        Assert.Contains("Status in Open", (string)chips[0].Content!, StringComparison.Ordinal);

        ViewHarness.Press(chips[0], Key.Delete);
        TestHost.Pump();
        Assert.Single(s.Model.Conditions);
        Assert.Equal("amount", s.Model.Conditions[0].Field);

        ViewHarness.Press(s.View.Chips.Children.OfType<SpotButton>().First(), Key.Back);
        TestHost.Pump();
        Assert.Empty(s.Model.Conditions);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task TheSearchBoxFiltersAfterTheDebounce()
    {
        var s = await ViewHarness.ShowAsync();

        s.View.Search.Text = "ana";

        Assert.True(await ViewHarness.WaitAsync(() => s.Model.AppliedSearch == "ana"));
        Assert.True(await ViewHarness.WaitAsync(() => s.Gateway.Requests[^1].Contains("\"contains\"", StringComparison.Ordinal)));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ADeniedListShowsTheReasonAndNoGrid()
    {
        var s = await ViewHarness.ShowAsync(new FakeListGateway(100, Forms.DocumentRights.None), settle: false);
        TestHost.Pump();

        Assert.Equal("You do not have access to this list.", s.View.StateText);
        Assert.False(s.View.Grid.IsVisible);
        Assert.Empty(s.Gateway.Requests);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AnEmptyListSaysSoAndAFilterWithNoMatchesSaysThat()
    {
        var s = await ViewHarness.ShowAsync(new FakeListGateway(0), settle: false);
        await s.Model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken);
        await ViewHarness.PumpAsync();
        Assert.Equal("There are no documents yet.", s.View.StateText);

        s.Gateway.FilteredRows = 0;
        s.Model.AddCondition(new FilterCondition("status", FilterOperator.OneOf, ["Void"]));
        await s.Model.Source.GetWindow(0, 10, TestContext.Current.CancellationToken);
        await ViewHarness.PumpAsync();
        Assert.Equal("No documents match the filter.", s.View.StateText);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AFailedQueryShowsTheServersMessageAndRetryWorks()
    {
        var s = await ViewHarness.ShowAsync(settle: false);
        s.Gateway.FailNext = new Api.ApiRequestException(Fixtures.Error("api.unavailable", "The server is busy", 503));
        s.Model.Refresh();
        await ViewHarness.PumpAsync(60);
        TestHost.Pump();

        Assert.Contains("The server is busy", s.View.StateText + s.View.StatusLine, StringComparison.Ordinal);

        ViewHarness.Press(s.View.Grid, Key.F5);
        Assert.True(await ViewHarness.SettleAsync(s.View));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ServerTextIsNeverInterpretedAsMarkup()
    {
        var s = await ViewHarness.ShowAsync(new FakeListGateway(10) { Prefix = "<Run Text='x'/> {0} " });

        Assert.Equal("<Run Text='x'/> {0} Order 0", ViewHarness.CellText(s.View.Grid, 0, 0));
        s.Window.Close();
    }

    private static async Task<(ListViewModel Model, ListSplitView Split, Avalonia.Controls.Window Window)> SplitAsync()
    {
        var model = new ListViewModel(new FakeListGateway(100), new FakeShell(), new InMemoryFilterStore(), "order", "ana", Fixtures.Options());
        await model.LoadAsync(TestContext.Current.CancellationToken);
        var split = new ListSplitView(new ListView(model), (_, id) => new Avalonia.Controls.TextBlock { Text = "form " + id });
        return (model, split, TestHost.Show(split));
    }

    [AvaloniaFact]
    public async Task TheSplitHookShowsTheSelectedRowBesideTheListOnlyBehindTheFlag()
    {
        var (plainModel, plain, plainWindow) = await SplitAsync();
        plainModel.SetSelection(["d-000004"]);
        Assert.Null(plain.Detail);
        plainWindow.Close();

        ListFlags.SplitDemo = true;
        try
        {
            var (model, split, window) = await SplitAsync();
            model.SetSelection(["d-000004"]);
            Assert.Equal("form d-000004", ((Avalonia.Controls.TextBlock)split.Detail!).Text);
            model.SetSelection(["d-000004", "d-000005"]);
            Assert.Null(split.Detail);
            window.Close();
        }
        finally
        {
            ListFlags.SplitDemo = false;
        }
    }
}
