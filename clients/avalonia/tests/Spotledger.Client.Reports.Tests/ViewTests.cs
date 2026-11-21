using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Api;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.Reports;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

public sealed class ViewTests
{
    private static FakeReportGateway Gateway(Action<FakeReportGateway>? configure = null)
    {
        var gateway = new FakeReportGateway { Description = Fx.Simple(parameters: 2) };
        configure?.Invoke(gateway);
        return gateway;
    }

    [AvaloniaFact]
    public async Task EveryParameterTypeGetsItsFieldControlAndRequiredOnesAreMarked()
    {
        var gateway = new FakeReportGateway { Description = ReportJson.ReadDescription(Fx.AllTypesDescription) };
        var s = await ViewHarness.ShowAsync(gateway);

        var controls = s.View.ParameterControls;

        Assert.IsType<SpotLookupBox>(controls["customer"]);
        Assert.IsType<SpotDateInput>(controls["from_date"]);
        Assert.IsType<SpotDateInput>(controls["to_date"]);
        Assert.IsType<SpotTextInput>(controls["min_amount"]);
        Assert.IsType<SpotTextInput>(controls["min_lines"]);
        Assert.IsType<SpotSelect>(controls["status"]);
        Assert.IsType<SpotTextInput>(controls["note"]);
        var fields = s.View.Form.Children.OfType<SpotField>().ToArray();
        Assert.Equal(7, fields.Length);
        Assert.Equal([true, true, false, false, false, false, false], fields.Select(f => f.IsRequired));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task DefaultsAreInTheControlsBeforeAnythingIsTyped()
    {
        var gateway = new FakeReportGateway { Description = ReportJson.ReadDescription(Fx.AllTypesDescription) };
        var s = await ViewHarness.ShowAsync(gateway);

        Assert.Equal("100.50", ((SpotTextInput)s.View.ParameterControls["min_amount"]).Text);
        Assert.Equal("3", ((SpotTextInput)s.View.ParameterControls["min_lines"]).Text);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task EnterInTheFormRunsTheReportWithWhatWasTyped()
    {
        var s = await ViewHarness.ShowAsync(Gateway());
        ViewHarness.TypeInto(s.View, "p0", "acme");

        ViewHarness.Press(s.View.ParameterControls["p0"], Key.Enter);

        Assert.True(await Pumping.WaitAsync(() => s.Gateway.Runs == 1 && s.Model.Phase == ReportPhase.Ready));
        Assert.Equal("""{"p0":"acme"}""", s.Gateway.Calls[0].Filters);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ControlEnterRunsFromAnywhereInTheTab()
    {
        var s = await ViewHarness.ShowAsync(Gateway());
        ViewHarness.TypeInto(s.View, "p0", "acme");

        ViewHarness.Press(s.View.Grid, Key.Enter, KeyModifiers.Control);

        Assert.True(await Pumping.WaitAsync(() => s.Gateway.Runs == 1));
        Assert.True(await ViewHarness.SettleAsync(s.View));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AParameterlessReportRunsOnOpenAndShowsItsRows()
    {
        var s = await ViewHarness.ShowAsync(Gateway(g => g.Description = Fx.Simple(parameters: 0)));

        Assert.True(await ViewHarness.SettleAsync(s.View));
        Assert.Equal("C00000", ViewHarness.CellText(s.View.Grid, 0, 0));
        Assert.Equal(3, s.View.Grid.RowCount);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task TheKeyboardAloneRunsTheReportAndExportsIt()
    {
        var sink = new MemorySink();
        var s = await ViewHarness.ShowAsync(Gateway(g => g.Result = Fx.Result(rows: 2)), new ReportViewModelOptions { ExportSink = sink });
        ViewHarness.TypeInto(s.View, "p0", "acme");

        ViewHarness.Press(s.View.ParameterControls["p0"], Key.Enter, KeyModifiers.Control);
        Assert.True(await ViewHarness.SettleAsync(s.View));
        ViewHarness.Press(s.View.Grid, Key.E, KeyModifiers.Control);

        Assert.True(await Pumping.WaitAsync(() => sink.Text.Length > 0));
        Assert.Equal("Party,Memo,Balance\r\nC00000,Row 0,0.50\r\nC00001,Row 1,1.50\r\n", sink.Text);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ControlEnterWhileARunIsInFlightMakesNoSecondRequest()
    {
        var release = new TaskCompletionSource();
        var s = await ViewHarness.ShowAsync(Gateway(g => g.OnRun = async (_, _, token) =>
        {
            await release.Task.WaitAsync(token);
            return Fx.Result();
        }));
        ViewHarness.TypeInto(s.View, "p0", "acme");

        for (var i = 0; i < 10; i++)
        {
            ViewHarness.Press(s.View.Grid, Key.Enter, KeyModifiers.Control);
        }

        await Pumping.PumpAsync();
        release.SetResult();
        Assert.True(await ViewHarness.SettleAsync(s.View));
        Assert.Equal(1, s.Gateway.Runs);
        Assert.Equal(1, s.Gateway.MaxInFlight);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task EscapeCancelsARunInFlight()
    {
        var s = await ViewHarness.ShowAsync(Gateway(g => g.OnRun = async (_, _, token) =>
        {
            await Task.Delay(Timeout.Infinite, token);
            return Fx.Result();
        }));
        ViewHarness.TypeInto(s.View, "p0", "acme");
        _ = s.View.RunAsync();
        Assert.True(await Pumping.WaitAsync(() => s.Model.Phase == ReportPhase.Running));

        ViewHarness.Press(s.View.Grid, Key.Escape);

        Assert.Equal(ReportPhase.Ready, s.Model.Phase);
        Assert.Equal("The run was cancelled.", s.View.MessageText);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task DecimalsAreRightAlignedWithTheirExactTextAndTheTotalRowIsLabelled()
    {
        var balances = new[] { "12345678901234567890123456789.01", "-0.01", "0.99" };
        var s = await ViewHarness.ShowAsync(Gateway(g => g.Result = Fx.Result(rows: 3, balances: balances, summable: true)));
        ViewHarness.TypeInto(s.View, "p0", "acme");
        await s.View.RunAsync();
        Assert.True(await ViewHarness.SettleAsync(s.View));

        Assert.True(s.View.Grid.TryGetRow(0, out var first));
        Assert.Equal("12345678901234567890123456789.01", first.Cells[2].Text);
        Assert.Equal(GridCellAlignment.End, first.Cells[2].Alignment);
        Assert.Equal(GridCellAlignment.End, s.View.Grid.Columns![2].DefaultAlignment);
        var footer = s.View.Grid.Footer!;
        Assert.Equal(ReportResultSource.TotalLabel, footer.Cells[0].Text);
        Assert.Equal("12345678901234567890123456789.99", footer.Cells[2].Text);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ACutResultShowsTheBannerWithNarrowingParametersAndNoTotalRow()
    {
        var s = await ViewHarness.ShowAsync(Gateway(g =>
        {
            g.Description = Fx.Simple(parameters: 3);
            g.Result = Fx.Result(rows: 4, truncated: true, summable: true);
        }));
        ViewHarness.TypeInto(s.View, "p0", "acme");
        await s.View.RunAsync();
        Assert.True(await ViewHarness.SettleAsync(s.View));

        Assert.Equal("Showing the first 4 rows. The report has more: narrow the parameters to see the rest. You can narrow it with: Param 1, Param 2.", s.View.TruncationText);
        Assert.Null(s.View.Grid.Footer);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task RemovedColumnsGetOneLineThatNamesNone()
    {
        var s = await ViewHarness.ShowAsync(Gateway(g => g.Result = Fx.Result(messages: [new ReportMessage("info", ReportJson.ColumnsRemovedCode, "2 columns were removed.")])));
        ViewHarness.TypeInto(s.View, "p0", "acme");
        await s.View.RunAsync();

        Assert.Equal("2 columns are not shown because you may not see them.", s.View.RemovedColumnsText);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AServerRefusalMarksTheFieldItNames()
    {
        var s = await ViewHarness.ShowAsync(Gateway(g => g.FailRun = new ApiRequestException(Fx.Error("report.bad_argument", "Bad.", 422, new ApiFieldError("p1", "report.bad_argument", "Whole number expected.")))));
        ViewHarness.TypeInto(s.View, "p0", "acme");

        await s.View.RunAsync();

        var field = s.View.Form.Children.OfType<SpotField>().Skip(1).First();
        Assert.Equal("Whole number expected.", field.ErrorText);
        Assert.Equal("Bad.", s.View.MessageText);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ARequiredParameterLeftEmptyIsMarkedWithoutARequest()
    {
        var s = await ViewHarness.ShowAsync(Gateway());

        await s.View.RunAsync();

        Assert.Equal(0, s.Gateway.Runs);
        Assert.Equal("Param 0 is required.", s.View.Form.Children.OfType<SpotField>().First().ErrorText);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task TheQuickFilterNarrowsTheGridAndTheStatusLine()
    {
        var s = await ViewHarness.ShowAsync(Gateway(g => g.Result = Fx.Result(rows: 30)));
        ViewHarness.TypeInto(s.View, "p0", "acme");
        await s.View.RunAsync();
        Assert.True(await ViewHarness.SettleAsync(s.View));

        s.Model.SetQuickFilter("Row 2");
        await Pumping.PumpAsync();

        Assert.Equal(11, s.View.Grid.RowCount);
        Assert.Equal("11 of 30 rows", s.View.StatusLine);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AFailedDescriptionFailsClosedAndShowsWhy()
    {
        var gateway = Gateway(g => g.FailDescribe = new ApiRequestException(Fx.Error("report.not_found", "No report.", 404)));
        var s = await ViewHarness.ShowAsync(gateway);

        Assert.Equal("This report is not available.", s.View.StateText);
        Assert.False(s.View.RunButton.IsEnabled);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ControlCCopiesTheSelectedRowsAsHardenedTsv()
    {
        var clipboard = new CaptureClipboard();
        var s = await ViewHarness.ShowAsync(Gateway(g => g.Result = Fx.Result(rows: 3, balances: ["1.00", "2.00", "3.00"])), new ReportViewModelOptions { Clipboard = clipboard });
        ViewHarness.TypeInto(s.View, "p0", "acme");
        await s.View.RunAsync();
        Assert.True(await ViewHarness.SettleAsync(s.View));
        s.View.Grid.SetSelection([1]);

        ViewHarness.Press(s.View.Grid, Key.C, KeyModifiers.Control);

        Assert.True(await Pumping.WaitAsync(() => clipboard.LastText is not null));
        Assert.Equal("C00001\tRow 1\t2.00\n", clipboard.LastText);
        s.Window.Close();
    }
}

public sealed class CatalogueViewTests
{
    private static async Task<(ReportCatalogueView View, ReportCatalogueViewModel Model, FakeReportShell Shell, Avalonia.Controls.Window Window)> ShowAsync(FakeReportGateway? gateway = null)
    {
        gateway ??= new FakeReportGateway();
        if (gateway.Reports.Count == 0)
        {
            gateway.Reports.AddRange(
            [
                new ReportSummary("trial_balance", "Trial balance", "accounts", "gl_entry"),
                new ReportSummary("aging", "Receivables aging", "accounts", "sales_invoice"),
                new ReportSummary("stock_ledger", "Stock ledger", "stock", "stock_entry"),
            ]);
        }

        var shell = new FakeReportShell();
        var model = new ReportCatalogueViewModel(gateway, shell, new InMemoryReportSettingsStore(), "ana");
        await model.LoadAsync(TestContext.Current.CancellationToken);
        var view = new ReportCatalogueView(model);
        return (view, model, shell, TestHost.Show(view, 700, 500));
    }

    [AvaloniaFact]
    public async Task TheListShowsAppHeadersAndReportsAndEnterOpensTheSelectedOne()
    {
        var s = await ShowAsync();

        Assert.Equal(["accounts", "   Receivables aging", "   Trial balance", "stock", "   Stock ledger"], s.View.RowTexts);
        s.View.List.SelectedIndex = 2;
        ViewHarness.Press(s.View.List, Key.Enter);

        Assert.Equal("trial_balance", Assert.Single(s.Shell.Opened).Name);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task SpaceStarsTheSelectedReport()
    {
        var s = await ShowAsync();
        s.View.List.SelectedIndex = 4;

        ViewHarness.Press(s.View.List, Key.Space);

        Assert.True(await Pumping.WaitAsync(() => s.View.RowTexts.Contains("★ Stock ledger")));
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AnEmptyOrUnreachableCatalogueIsOneLine()
    {
        var empty = await ShowAsync(new FakeReportGateway { Reports = { } });
        var failing = new FakeReportGateway { FailList = new ApiRequestException(ApiError.TransportFailure(0, "x")) };
        var shell = new FakeReportShell();
        var model = new ReportCatalogueViewModel(failing, shell, new InMemoryReportSettingsStore(), "ana");
        await model.LoadAsync(TestContext.Current.CancellationToken);
        using var view = new ReportCatalogueView(model);
        var window = TestHost.Show(view, 600, 400);

        Assert.Equal("The server cannot be reached, so the reports cannot be listed.", view.StateText);
        window.Close();
        empty.Window.Close();
    }
}
