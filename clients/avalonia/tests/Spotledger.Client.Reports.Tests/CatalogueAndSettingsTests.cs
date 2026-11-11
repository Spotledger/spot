using Spotledger.Client.Api;
using Spotledger.Client.Reports;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

public sealed class CatalogueAndSettingsTests
{
    private static FakeReportGateway Gateway()
    {
        var gateway = new FakeReportGateway();
        gateway.Reports.AddRange(
        [
            new ReportSummary("trial_balance", "Trial balance", "accounts", "gl_entry"),
            new ReportSummary("aging", "Receivables aging", "accounts", "sales_invoice"),
            new ReportSummary("stock_ledger", "Stock ledger", "stock", "stock_entry"),
            new ReportSummary("sales_by_customer", "Sales by customer", "selling", "sales_invoice"),
        ]);
        return gateway;
    }

    private static async Task<(ReportCatalogueViewModel Model, FakeReportShell Shell, InMemoryReportSettingsStore Store)> LoadedAsync(FakeReportGateway? gateway = null, ReportUserSettings? seed = null)
    {
        var store = new InMemoryReportSettingsStore();
        if (seed is not null)
        {
            await store.SaveAsync("ana", seed, TestContext.Current.CancellationToken);
        }

        var shell = new FakeReportShell();
        var model = new ReportCatalogueViewModel(gateway ?? Gateway(), shell, store, "ana");
        await model.LoadAsync(TestContext.Current.CancellationToken);
        return (model, shell, store);
    }

    [Fact]
    public async Task ReportsAreGroupedByAppAndOrderedByLabel()
    {
        var (model, _, _) = await LoadedAsync();

        Assert.Equal(CataloguePhase.Ready, model.Phase);
        Assert.Equal(["accounts", "selling", "stock"], model.Groups.Select(g => g.App));
        Assert.Equal(["Receivables aging", "Trial balance"], model.Groups[0].Entries.Select(e => e.Summary.Label));
    }

    [Fact]
    public async Task RecentlyOpenedReportsComeFirstInTheirGroup()
    {
        var (model, _, _) = await LoadedAsync(seed: new ReportUserSettings([], ["trial_balance"], new Dictionary<string, IReadOnlyList<ReportParameterSet>>()));

        Assert.Equal(["Trial balance", "Receivables aging"], model.Groups[0].Entries.Select(e => e.Summary.Label));
        Assert.True(model.Groups[0].Entries[0].IsRecent);
    }

    [Fact]
    public async Task SearchMatchesTheLabelIgnoringCaseAndDropsEmptyGroups()
    {
        var (model, _, _) = await LoadedAsync();

        model.SetSearch("  LEDGER ");

        Assert.Equal(["stock"], model.Groups.Select(g => g.App));
        Assert.Equal(1, model.ListedCount);
        model.SetSearch("nothing like this");
        Assert.Equal("No report matches.", model.Message);
    }

    [Fact]
    public async Task AFavouriteIsRememberedPerPersonAndFiltersTheList()
    {
        var (model, _, store) = await LoadedAsync();

        await model.ToggleFavouriteAsync("stock_ledger", TestContext.Current.CancellationToken);
        model.SetOnlyFavourites(true);

        Assert.Equal(["Stock ledger"], model.Groups.SelectMany(g => g.Entries).Select(e => e.Summary.Label));
        Assert.Equal(["stock_ledger"], (await store.LoadAsync("ana", TestContext.Current.CancellationToken)).Favourites);
        Assert.Empty((await store.LoadAsync("ben", TestContext.Current.CancellationToken)).Favourites);
        await model.ToggleFavouriteAsync("stock_ledger", TestContext.Current.CancellationToken);
        Assert.Empty(model.Groups);
    }

    [Fact]
    public async Task AnEmptyCatalogueSaysSoInOneLine()
    {
        var (model, _, _) = await LoadedAsync(new FakeReportGateway());

        Assert.Equal(CataloguePhase.Empty, model.Phase);
        Assert.Equal("No reports are available to you.", model.Message);
    }

    [Fact]
    public async Task AnUnreachableServerSaysSoInOneLine()
    {
        var gateway = new FakeReportGateway { FailList = new ApiRequestException(ApiError.TransportFailure(0, "boom")) };

        var (model, _, _) = await LoadedAsync(gateway);

        Assert.Equal(CataloguePhase.Unreachable, model.Phase);
        Assert.Equal("The server cannot be reached, so the reports cannot be listed.", model.Message);
    }

    [Fact]
    public async Task OpeningAReportAsksTheShellForItsTab()
    {
        var (model, shell, _) = await LoadedAsync();

        Assert.True(model.Open("aging"));
        Assert.False(model.Open("missing"));

        var opened = Assert.Single(shell.Opened);
        Assert.Equal(("aging", "Receivables aging"), (opened.Name, opened.Label));
    }

    [Fact]
    public void SettingsRoundTripThroughJsonAndDropWhatIsNotAnIdentifier()
    {
        var sets = new Dictionary<string, IReadOnlyList<ReportParameterSet>>
        {
            ["aging"] = [new ReportParameterSet("Month end", new Dictionary<string, string> { ["as_of"] = "2026-09-30", ["bad name"] = "x" })],
            ["../etc"] = [new ReportParameterSet("evil", new Dictionary<string, string>())],
        };
        var settings = new ReportUserSettings(["aging", "not valid", "aging"], ["stock_ledger"], sets);

        var back = ReportUserSettings.FromJson(settings.ToJson());

        Assert.Equal(["aging"], back.Favourites);
        Assert.Equal(["stock_ledger"], back.Recent);
        var set = Assert.Single(back.Sets["aging"]);
        Assert.Equal("Month end", set.Name);
        Assert.Equal(["as_of"], set.Values.Keys);
        Assert.False(back.Sets.ContainsKey("../etc"));
    }

    [Fact]
    public void MalformedOrOversizedSettingsGiveEmptyOnes()
    {
        Assert.Empty(ReportUserSettings.FromJson("{not json").Favourites);
        Assert.Empty(ReportUserSettings.FromJson(new string(' ', ReportUserSettings.MaxBytes + 1)).Favourites);
        Assert.Empty(ReportUserSettings.FromJson(null).Favourites);
    }

    [Fact]
    public async Task TheFileStoreKeepsOneFilePerPersonUnderAHashedName()
    {
        var directory = Path.Combine(Path.GetTempPath(), "spot-reports-" + Guid.NewGuid().ToString("N"));
        try
        {
            var store = new FileReportSettingsStore(directory);
            await store.SaveAsync("ana@example.com", new ReportUserSettings(["aging"], [], new Dictionary<string, IReadOnlyList<ReportParameterSet>>()), TestContext.Current.CancellationToken);

            var again = await new FileReportSettingsStore(directory).LoadAsync("ana@example.com", TestContext.Current.CancellationToken);
            var other = await store.LoadAsync("ben@example.com", TestContext.Current.CancellationToken);

            Assert.Equal(["aging"], again.Favourites);
            Assert.Empty(other.Favourites);
            var file = Assert.Single(Directory.GetFiles(directory));
            Assert.DoesNotContain("ana", Path.GetFileName(file), StringComparison.Ordinal);
        }
        finally
        {
            if (Directory.Exists(directory))
            {
                Directory.Delete(directory, recursive: true);
            }
        }
    }

    [Fact]
    public async Task ParameterSetsSaveApplyAndDeleteAndSurviveANewTab()
    {
        var store = new InMemoryReportSettingsStore();
        var gateway = new FakeReportGateway { Description = Fx.Simple(parameters: 3) };
        var model = Fx.Model(gateway, store: store);
        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);
        model.Parameters!.Items[0].TrySetText("acme");
        model.Parameters.Items[1].TrySetText("12");
        model.Parameters.Items[2].TrySetText("0.5");

        Assert.True(await model.SaveParameterSetAsync("Acme monthly", TestContext.Current.CancellationToken));
        model.ResetParameters();
        Assert.Equal(["", "", ""], model.Parameters.Items.Select(p => p.Text));
        Assert.True(model.ApplyParameterSet("Acme monthly"));
        Assert.Equal(["acme", "12", "0.5"], model.Parameters.Items.Select(p => p.Text));

        var second = Fx.Model(gateway, store: store);
        await second.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal(["Acme monthly"], second.ParameterSets.Select(s => s.Name));

        Assert.True(await second.DeleteParameterSetAsync("Acme monthly", TestContext.Current.CancellationToken));
        Assert.Empty(second.ParameterSets);
        Assert.False(second.ApplyParameterSet("Acme monthly"));
    }

    [Fact]
    public async Task SavingASetUnderTheSameNameReplacesItAndTheNameIsAskedForWhenMissing()
    {
        var shell = new FakeReportShell { Text = "  Weekly  " };
        var store = new InMemoryReportSettingsStore();
        var model = Fx.Model(new FakeReportGateway(), shell, store);
        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);
        model.Parameters!.Items[0].TrySetText("one");
        await model.SaveParameterSetAsync(cancellationToken: TestContext.Current.CancellationToken);
        model.Parameters.Items[0].TrySetText("two");

        await model.SaveParameterSetAsync(cancellationToken: TestContext.Current.CancellationToken);

        var set = Assert.Single(model.ParameterSets);
        Assert.Equal("Weekly", set.Name);
        Assert.Equal("two", set.Values["p0"]);
        shell.Text = null;
        Assert.False(await model.SaveParameterSetAsync(cancellationToken: TestContext.Current.CancellationToken));
    }

    [Fact]
    public async Task ASavedValueThatNoLongerFitsIsLeftEmptyAndReported()
    {
        var store = new InMemoryReportSettingsStore();
        await store.SaveAsync("ana", new ReportUserSettings([], [], new Dictionary<string, IReadOnlyList<ReportParameterSet>>
        {
            ["aging"] = [new ReportParameterSet("Old", new Dictionary<string, string> { ["p0"] = "acme", ["p1"] = "not a number" })],
        }), TestContext.Current.CancellationToken);
        var model = Fx.Model(new FakeReportGateway(), store: store);
        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);

        model.ApplyParameterSet("Old");

        Assert.Equal("acme", model.Parameters!.Items[0].Text);
        Assert.Equal("Some saved values no longer fit: Param 1.", model.Message);
        Assert.NotNull(model.Parameters.Items[1].Error);
    }
}

public sealed class TabAndCommandTests
{
    [Fact]
    public void TheSameReportWithTheSameParametersFocusesTheOpenTab()
    {
        var kinds = new WorkspaceTabKinds();
        var made = 0;
        ReportTabs.Register(kinds, () => throw new InvalidOperationException(), (name, parameters) =>
        {
            made++;
            return new ReportViewModel(new FakeReportGateway(), new FakeReportShell(), new InMemoryReportSettingsStore(), name, "ana");
        });
        var request = ReportTabs.ReportRequest("aging", "Aging", new Dictionary<string, string> { ["as_of"] = "2026-09-30", ["empty"] = string.Empty });

        var first = kinds.Open(request);
        var again = kinds.Open(ReportTabs.ReportRequest("aging", "Aging", new Dictionary<string, string> { ["as_of"] = "2026-09-30" }));
        var other = kinds.Open(ReportTabs.ReportRequest("aging", "Aging", new Dictionary<string, string> { ["as_of"] = "2026-10-31" }));
        var plain = kinds.Open(ReportTabs.ReportRequest("aging"));

        Assert.Same(first, again);
        Assert.NotSame(first, other);
        Assert.NotSame(first, plain);
        Assert.Equal(3, made);
    }

    [Fact]
    public void ARequestCarriesTheReportAndItsStartingParameters()
    {
        var request = ReportTabs.ReportRequest("aging", "Aging", new Dictionary<string, string> { ["as_of"] = "2026-09-30" });

        var (report, parameters) = ReportTabs.ParseReportRequest(request);

        Assert.Equal("aging", report);
        Assert.Equal("2026-09-30", parameters["as_of"]);
        Assert.Equal(ReportTabs.ReportKind, request.Kind);
        Assert.Equal("Aging", request.Title);
    }

    [Fact]
    public void TheCatalogueIsOneTab()
    {
        var kinds = new WorkspaceTabKinds();
        ReportTabs.Register(kinds, () => new ReportCatalogueViewModel(new FakeReportGateway(), new FakeReportShell(), new InMemoryReportSettingsStore(), "ana"),
            (_, _) => throw new InvalidOperationException());

        var a = kinds.Open(ReportTabs.CatalogueRequest());
        var b = kinds.Open(ReportTabs.CatalogueRequest());

        Assert.Same(a, b);
    }

    [Fact]
    public void TheLauncherOffersOpenReportCommandsThatResolveToTheTab()
    {
        var definition = ReportCommands.Open(new ReportSummary("aging", "Receivables aging", "accounts", "sales_invoice"));

        Assert.Equal("report.open.aging", definition.Id);
        Assert.Equal("Open report Receivables aging", definition.Label);
        Assert.Equal("Reports", definition.Category);
        Assert.Equal("aging", ReportCommands.TryParse(definition.Id));
        Assert.Null(ReportCommands.TryParse("document.new.invoice"));
        Assert.Equal(ReportTabs.ReportRequest("aging", "Aging").Key, ReportCommands.RequestFor(definition.Id, "Aging")!.Key);
    }

    [Fact]
    public async Task TheTabTitleFollowsTheReportsLabel()
    {
        var model = Fx.Model(new FakeReportGateway());
        var tab = new WorkspaceTabViewModel("t1", model.Title);
        using var binding = ReportTabs.Bind(tab, model);

        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);

        Assert.Equal("Receivables aging", tab.Title);
    }
}
