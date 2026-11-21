using Spotledger.Client.Api;
using Spotledger.Client.Reports;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

public sealed class RunTests
{
    private static async Task<(ReportViewModel Model, FakeReportGateway Gateway)> LoadedAsync(Action<FakeReportGateway>? configure = null, ReportViewModelOptions? options = null)
    {
        var gateway = new FakeReportGateway();
        configure?.Invoke(gateway);
        var model = Fx.Model(gateway, options: options);
        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);
        return (model, gateway);
    }

    [Fact]
    public async Task LoadingReadsTheDescriptionAndBuildsTheFormWithoutRunningAReportThatNeedsParameters()
    {
        var (model, gateway) = await LoadedAsync();

        Assert.Equal(ReportPhase.Ready, model.Phase);
        Assert.Equal("Receivables aging", model.Title);
        Assert.Equal(2, model.Parameters!.Items.Count);
        Assert.Equal(0, gateway.Runs);
        Assert.Null(model.Source);
        Assert.Equal("Set the parameters and run the report.", model.StatusText);
    }

    [Fact]
    public async Task AReportWithoutParametersRunsOnOpen()
    {
        var (model, gateway) = await LoadedAsync(g => g.Description = Fx.Simple(parameters: 0));

        Assert.Equal(1, gateway.Runs);
        Assert.Equal(3, model.Source!.LoadedRows);
        Assert.Equal("""{}""", gateway.Calls[0].Filters);
    }

    [Fact]
    public async Task ARunWithAMissingRequiredParameterNeverReachesTheServer()
    {
        var (model, gateway) = await LoadedAsync();

        await model.RunAsync();

        Assert.Equal(0, gateway.Runs);
        Assert.Equal("Param 0 is required.", model.Parameters!.Items[0].Error);
        Assert.Equal(ReportPhase.Ready, model.Phase);
        Assert.NotNull(model.Message);
    }

    [Fact]
    public async Task AValidRunSendsTheFiltersInTheContractEncodingAndTheDescriptionsRowLimit()
    {
        var (model, gateway) = await LoadedAsync(g => g.Description = Fx.Simple(parameters: 4, maxRows: 777));
        model.Parameters!.Items[0].TrySetText("acme");
        model.Parameters.Items[1].TrySetText("12");
        model.Parameters.Items[2].TrySetText("0.125");
        model.Parameters.Items[3].TrySetText("B");

        await model.RunAsync();

        var call = Assert.Single(gateway.Calls);
        Assert.Equal("aging", call.Report);
        Assert.Equal("""{"p0":"acme","p1":12,"p2":"0.125","p3":"B"}""", call.Filters);
        Assert.Equal(777, call.Limit);
        Assert.Equal("3 rows", model.StatusText);
    }

    [Fact]
    public async Task AServerRefusalIsShownOnTheParameterItNamesAndNoResultIsKept()
    {
        var (model, gateway) = await LoadedAsync();
        model.Parameters!.Items[0].TrySetText("acme");
        await model.RunAsync();
        Assert.NotNull(model.Source);
        gateway.FailRun = new ApiRequestException(Fx.Error("report.bad_argument", "p1 must be a whole number.", 422, new ApiFieldError("p1", "report.bad_argument", "Param 1 must be a whole number.")));

        await model.RunAsync();

        Assert.Equal("Param 1 must be a whole number.", model.Parameters.Items[1].Error);
        Assert.Null(model.Source);
        Assert.Equal(ReportPhase.Ready, model.Phase);
        Assert.Equal("p1 must be a whole number.", model.Message);
    }

    [Fact]
    public async Task ANotFoundReportFailsClosedWithoutNamingWhyItIsMissing()
    {
        var gateway = new FakeReportGateway { FailDescribe = new ApiRequestException(Fx.Error("report.not_found", "No report with this name.", 404)) };
        var model = Fx.Model(gateway);

        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);

        Assert.Equal(ReportPhase.Failed, model.Phase);
        Assert.Equal("This report is not available.", model.StatusText);
        Assert.False(model.CanRun);
    }

    [Fact]
    public async Task AParameterKindThisClientDoesNotKnowFailsClosed()
    {
        var gateway = new FakeReportGateway
        {
            Description = Fx.Simple(0) with { Parameters = [new ReportParameterDef("g", "Geo", ReportParameterType.Unknown, false, null, null)] },
        };
        var model = Fx.Model(gateway);

        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);

        Assert.Equal(ReportPhase.Failed, model.Phase);
        Assert.Equal(0, gateway.Runs);
    }

    [Fact]
    public async Task ACutResultSaysSoWithTheRowCountOffersNarrowingParametersAndShowsNoTotal()
    {
        var (model, _) = await LoadedAsync(g =>
        {
            g.Description = Fx.Simple(parameters: 3);
            g.Result = Fx.Result(rows: 5, truncated: true, summable: true);
        });
        model.Parameters!.Items[0].TrySetText("acme");

        await model.RunAsync();

        Assert.True(model.Truncated);
        Assert.Equal("Showing the first 5 rows. The report has more: narrow the parameters to see the rest.", model.TruncationBanner);
        Assert.Equal(["Param 1", "Param 2"], model.NarrowingParameters);
        Assert.Null(model.Source!.BuildFooter());
    }

    [Fact]
    public async Task AWholeResultHasNoBannerAndShowsTheTotalOfLoadedRows()
    {
        var (model, _) = await LoadedAsync(g => g.Result = Fx.Result(rows: 3, balances: ["10.10", "20.20", "-5.05"], summable: true));
        model.Parameters!.Items[0].TrySetText("acme");

        await model.RunAsync();

        Assert.Null(model.TruncationBanner);
        var footer = model.Source!.BuildFooter()!;
        Assert.Equal(ReportResultSource.TotalLabel, footer.Cells[0].Text);
        Assert.Equal("25.25", footer.Cells[2].Text);
    }

    [Fact]
    public async Task RemovedColumnsAreCountedFromTheMessagesWithoutNamingThem()
    {
        var (model, _) = await LoadedAsync(g => g.Result = Fx.Result(messages: [new ReportMessage("info", ReportJson.ColumnsRemovedCode, "3 columns were removed for you.")]));
        model.Parameters!.Items[0].TrySetText("acme");

        await model.RunAsync();

        Assert.Equal(3, model.RemovedColumns);
        Assert.Equal("3 columns are not shown because you may not see them.", model.RemovedColumnsNote);
        Assert.DoesNotContain("balance", model.RemovedColumnsNote, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public async Task OneRemovedColumnIsSingularAndNoMessageMeansNoNote()
    {
        var (model, gateway) = await LoadedAsync(g => g.Result = Fx.Result(messages: [new ReportMessage("info", ReportJson.ColumnsRemovedCode, "A column was removed.")]));
        model.Parameters!.Items[0].TrySetText("acme");
        await model.RunAsync();
        Assert.Equal("1 column is not shown because you may not see it.", model.RemovedColumnsNote);

        gateway.Result = Fx.Result();
        await model.RunAsync();

        Assert.Null(model.RemovedColumnsNote);
    }

    [Fact]
    public async Task OtherMessagesAreKeptAsPlainNotes()
    {
        var (model, _) = await LoadedAsync(g => g.Result = Fx.Result(messages: [new ReportMessage("warning", "report.slow", "<b>Slow</b> report")]));
        model.Parameters!.Items[0].TrySetText("acme");

        await model.RunAsync();

        Assert.Equal(["<b>Slow</b> report"], model.Notes);
    }

    [Fact]
    public async Task ARunInFlightIsJoinedNotRepeated()
    {
        var release = new TaskCompletionSource();
        var (model, gateway) = await LoadedAsync(g => g.OnRun = async (_, _, token) =>
        {
            await release.Task.WaitAsync(token);
            return Fx.Result();
        });
        model.Parameters!.Items[0].TrySetText("acme");

        var tasks = Enumerable.Range(0, 10).Select(_ => model.RunAsync()).ToArray();
        Assert.Equal(ReportPhase.Running, model.Phase);
        release.SetResult();
        await Task.WhenAll(tasks);

        Assert.Equal(1, gateway.Runs);
        Assert.Equal(1, gateway.MaxInFlight);
        Assert.Equal(ReportPhase.Ready, model.Phase);
    }

    [Fact]
    public async Task ACancelledRunIsForgottenAndItsLateAnswerIsDropped()
    {
        var late = new TaskCompletionSource<ReportRunResult>();
        var calls = 0;
        var (model, gateway) = await LoadedAsync(g => g.OnRun = (_, _, _) =>
            ++calls == 1 ? late.Task : Task.FromResult(Fx.Result(rows: 2)));
        model.Parameters!.Items[0].TrySetText("acme");
        var first = model.RunAsync();
        Assert.Equal(ReportPhase.Running, model.Phase);

        model.CancelRun();
        Assert.Equal(ReportPhase.Ready, model.Phase);
        Assert.Equal("The run was cancelled.", model.Message);
        await model.RunAsync();
        Assert.Equal(2, model.Source!.LoadedRows);

        late.SetResult(Fx.Result(rows: 99));
        await first;

        Assert.Equal(2, model.Source!.LoadedRows);
        Assert.Equal(2, gateway.Runs);
    }

    [Fact]
    public async Task ACancelledRunThatTheServerAbortsLeavesNothingBehind()
    {
        var (model, _) = await LoadedAsync(g => g.OnRun = async (_, _, token) =>
        {
            await Task.Delay(Timeout.Infinite, token);
            return Fx.Result();
        });
        model.Parameters!.Items[0].TrySetText("acme");
        var run = model.RunAsync();

        model.CancelRun();
        await run;

        Assert.Null(model.Source);
        Assert.Equal(ReportPhase.Ready, model.Phase);
    }

    [Fact]
    public async Task AServerErrorShowsItsMessageAndAllowsATryAgain()
    {
        var (model, gateway) = await LoadedAsync();
        model.Parameters!.Items[0].TrySetText("acme");
        gateway.FailRun = new ApiRequestException(Fx.Error("api.unavailable", "The server is busy.", 503));

        await model.RunAsync();
        Assert.Equal("The server is busy.", model.Message);
        Assert.True(model.CanRun);
        gateway.FailRun = null;
        await model.RunAsync();

        Assert.Null(model.Message);
        Assert.NotNull(model.Source);
    }

    [Fact]
    public async Task TheQuickFilterNarrowsTheLoadedRowsAndTheStatusSaysHowMany()
    {
        var (model, _) = await LoadedAsync(g => g.Result = Fx.Result(rows: 20));
        model.Parameters!.Items[0].TrySetText("acme");
        await model.RunAsync();

        model.SetQuickFilter("Row 1");

        Assert.Equal(11, model.Source!.ShownRows);
        Assert.Equal("11 of 20 rows", model.StatusText);
    }

    [Fact]
    public async Task ExportWritesTheShownRowsToTheSinkAClientSideCsv()
    {
        var sink = new MemorySink();
        var (model, gateway) = await LoadedAsync(g => g.Result = Fx.Result(rows: 3), new ReportViewModelOptions { ExportSink = sink });
        model.Parameters!.Items[0].TrySetText("acme");
        await model.RunAsync();
        model.SetQuickFilter("Row 2");

        var done = await model.ExportCsvAsync(TestContext.Current.CancellationToken);

        Assert.True(done);
        Assert.Equal("aging.csv", sink.Name);
        Assert.Equal("Party,Memo,Balance\r\nC00002,Row 2,2.50\r\n", sink.Text);
        Assert.Equal(0, gateway.ServerExports);
        Assert.Equal("Exported 1 rows.", model.Message);
    }

    [Fact]
    public async Task ExportUsesTheServersOperationWhenTheReportMapsToOne()
    {
        var sink = new MemorySink();
        var (model, gateway) = await LoadedAsync(g => g.ServerExport = true, new ReportViewModelOptions { ExportSink = sink });
        model.Parameters!.Items[0].TrySetText("acme");
        await model.RunAsync();

        await model.ExportCsvAsync(TestContext.Current.CancellationToken);

        Assert.Equal(1, gateway.ServerExports);
        Assert.Equal("server,export\r\n", sink.Text);
    }

    [Fact]
    public async Task ACancelledSaveDialogExportsNothing()
    {
        var sink = new MemorySink { Cancel = true };
        var (model, _) = await LoadedAsync(options: new ReportViewModelOptions { ExportSink = sink });
        model.Parameters!.Items[0].TrySetText("acme");
        await model.RunAsync();

        Assert.False(await model.ExportCsvAsync(TestContext.Current.CancellationToken));
    }

    [Fact]
    public async Task CopyingTheSelectionGivesHardenedTabSeparatedRows()
    {
        var clipboard = new CaptureClipboard();
        var (model, _) = await LoadedAsync(g => g.Result = Fx.Result(rows: 3, balances: ["1.00", "2.00", "3.00"]), new ReportViewModelOptions { Clipboard = clipboard });
        model.Parameters!.Items[0].TrySetText("acme");
        await model.RunAsync();

        var text = await model.CopySelectionAsync([2, 0], TestContext.Current.CancellationToken);

        Assert.Equal("C00000\tRow 0\t1.00\nC00002\tRow 2\t3.00\n", text);
        Assert.Equal(text, clipboard.LastText);
    }

    [Fact]
    public async Task OpeningAReportRecordsItAsRecentForThePerson()
    {
        var store = new InMemoryReportSettingsStore();
        var gateway = new FakeReportGateway();
        var model = Fx.Model(gateway, store: store);

        await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);

        Assert.Equal(["aging"], (await store.LoadAsync("ana", TestContext.Current.CancellationToken)).Recent);
    }

    [Fact]
    public async Task StartingValuesFromATabRequestFillTheForm()
    {
        var gateway = new FakeReportGateway();
        var model = Fx.Model(gateway);

        await model.LoadAsync(new Dictionary<string, string> { ["p0"] = "acme", ["p1"] = "7" }, TestContext.Current.CancellationToken);

        Assert.Equal(["acme", "7"], model.Parameters!.Items.Select(p => p.Text));
        Assert.Equal(ReportTabs.KeyFor("aging", ReportParameters.DigestOf(new Dictionary<string, string> { ["p0"] = "acme", ["p1"] = "7" })), model.TabKey);
    }
}
