using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Spotledger.Designer.Reports;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

/// <summary>The report builder view, shown in a headless window and driven through its controls.</summary>
public sealed class ReportBuilderViewTests
{
    [AvaloniaFact]
    public async Task The_view_lists_the_fields_and_every_section_the_builder_claims()
    {
        var shown = await Views.ReportAsync(new FakeBuilderServer());

        Assert.Equal(shown.Model.FieldChoices.Count, shown.Find<ListBox>("Palette").ItemCount);
        Assert.Contains("Customer > Territory", shown.Model.FieldChoices.Select(c => c.DisplayName));
        foreach (var heading in new[] { "Report on", "Fields", "Heading", "Columns", "Chart", "Filters", "Share with", "To fix or check", "Preview", "Run the published report" })
        {
            Assert.Contains(heading, shown.Texts());
        }

        foreach (var name in new[] { "UndoButton", "RedoButton", "SaveButton", "PublishButton", "RunButton", "UseBaseButton", "AddColumnButton", "GroupButton", "SortAscButton", "SortDescButton", "SetAggregateButton", "TotalButton", "SetChartButton", "ClearChartButton", "RemoveColumnButton", "RemoveFilterButton", "AddFilterButton", "SetLabelButton", "ShareButton" })
        {
            Assert.NotNull(shown.Find<Button>(name));
        }
    }

    [AvaloniaFact]
    public async Task A_grouped_report_with_a_total_a_chart_a_filter_and_a_share_is_built_by_clicking()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.ReportAsync(server);
        var palette = shown.Find<ListBox>("Palette");
        var model = shown.Model;

        // Pick the base record type from the search.
        shown.Find<TextBox>("BaseSearch").Text = "Sales";
        Assert.True(await Pumping.WaitAsync(() => model.BaseChoices.Count == 1));
        shown.Find<ListBox>("BaseList").SelectedIndex = 0;
        Clicks.On(shown.Find<Button>("UseBaseButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Base == "Sales Invoice"));

        // Heading.
        shown.Find<TextBox>("LabelBox").Text = "Sales by territory";
        Clicks.On(shown.Find<Button>("SetLabelButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Label == "Sales by territory"));

        // Columns: a field of the base and a field reached through the Customer link.
        palette.SelectedItem = Support.Choice(model, "customer.territory");
        Clicks.On(shown.Find<Button>("AddColumnButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Columns.Count == 1));
        palette.SelectedItem = Support.Choice(model, "grand_total");
        Clicks.On(shown.Find<Button>("AddColumnButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Columns.Count == 2));
        Assert.Single(model.Draft!.Joins);

        // Group by the territory.
        palette.SelectedItem = Support.Choice(model, "customer.territory");
        Clicks.On(shown.Find<Button>("GroupButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.GroupBy.Contains("customer.territory")));
        Assert.Contains("Grouped by customer.territory", shown.Texts());

        // Summarise and total the amount column.
        var columns = shown.Find<ListBox>("ColumnList");
        Assert.True(await Pumping.WaitAsync(() => columns.ItemCount == 2));
        columns.SelectedIndex = 1;
        shown.Find<ComboBox>("AggregateBox").SelectedItem = Aggregate.Sum;
        Clicks.On(shown.Find<Button>("SetAggregateButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Columns[1].Aggregate == Aggregate.Sum));
        columns.SelectedIndex = 1;
        Clicks.On(shown.Find<Button>("TotalButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Totals.Contains("grand_total")));

        // Sort.
        palette.SelectedItem = Support.Choice(model, "grand_total");
        Clicks.On(shown.Find<Button>("SortDescButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Sort.Count == 1));

        // A bar chart across the territory.
        shown.Find<ComboBox>("ChartKindBox").SelectedItem = ChartKind.Bar;
        shown.Find<ComboBox>("ChartXBox").SelectedItem = "customer.territory";
        var series = shown.Find<ListBox>("ChartSeriesList");
        series.SelectedItems!.Add("grand_total");
        Clicks.On(shown.Find<Button>("SetChartButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Chart is { Kind: ChartKind.Bar }));
        Assert.Equal(["grand_total"], model.Draft!.Chart!.Series);

        // A filter the viewer may change, which also defines a parameter.
        palette.SelectedItem = Support.Choice(model, "status");
        shown.Find<TextBox>("FilterBox").Text = "Paid";
        shown.Find<CheckBox>("FilterIsParameter").IsChecked = true;
        Clicks.On(shown.Find<Button>("AddFilterButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Filters.Count == 1 && model.Draft.Parameters.Count == 1));
        Assert.Single(shown.Find<ListBox>("FilterList").Items.Cast<object>());

        // Share, publish and run with the parameter.
        shown.Find<TextBox>("RolesBox").Text = "Accounts Manager, Sales Manager";
        Clicks.On(shown.Find<Button>("ShareButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Roles.Count == 2));
        Assert.True(await Pumping.WaitAsync(() => shown.Find<Button>("PublishButton").Command!.CanExecute(null)));
        Clicks.On(shown.Find<Button>("PublishButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Texts().Contains("Published sales_report.")));

        Assert.Single(shown.Find<ItemsControl>("ParameterList").Items.Cast<object>());
        model.ParameterInputs[0].Text = "Open";
        Clicks.On(shown.Find<Button>("RunButton"));
        Assert.True(await Pumping.WaitAsync(() => model.RunResult.Rows.Count > 0));
        Assert.Equal(new FilterValue.Text("Open"), server.LastRunParameters["status"]);
    }

    [AvaloniaFact]
    public async Task A_removed_column_and_a_removed_filter_leave_the_view()
    {
        var shown = await Views.ReportAsync(new FakeBuilderServer());
        var model = shown.Model;
        await model.AddColumnAsync(Support.Choice(model, "status"));
        await model.AddFilterAsync(ReportBuilderViewModel.NewFilter(Support.Choice(model, "status")) is { } f ? WithValue(f) : null!);
        var columns = shown.Find<ListBox>("ColumnList");
        Assert.True(await Pumping.WaitAsync(() => columns.ItemCount == 1));
        columns.SelectedIndex = 0;
        Clicks.On(shown.Find<Button>("RemoveColumnButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Columns.Count == 0));

        var filters = shown.Find<ListBox>("FilterList");
        Assert.True(await Pumping.WaitAsync(() => filters.ItemCount == 1));
        filters.SelectedIndex = 0;
        Clicks.On(shown.Find<Button>("RemoveFilterButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Filters.Count == 0));
    }

    private static FilterEditor WithValue(FilterEditor editor)
    {
        editor.ValueText = "Paid";
        return editor;
    }

    [AvaloniaFact]
    public async Task Undo_and_redo_buttons_walk_the_servers_op_log()
    {
        var shown = await Views.ReportAsync(new FakeBuilderServer());
        var model = shown.Model;
        await model.AddColumnAsync(Support.Choice(model, "status"));
        Clicks.On(shown.Find<Button>("UndoButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Columns.Count == 0));
        Clicks.On(shown.Find<Button>("RedoButton"));
        Assert.True(await Pumping.WaitAsync(() => model.Draft!.Columns.Count == 1));
    }

    [AvaloniaFact]
    public async Task A_dropped_connection_is_shown_in_the_status_line_not_swallowed()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.ReportAsync(server);
        var status = shown.Find<TextBlock>("StatusText");
        shown.Find<ListBox>("Palette").SelectedItem = Support.Choice(shown.Model, "status");

        server.Offline = true;
        Clicks.On(shown.Find<Button>("AddColumnButton"));
        Assert.True(await Clicks.StatusContainsAsync(status, "That did not work"));
        Assert.Empty(shown.Model.Draft!.Columns);

        // The same for a command button (Publish) whose failure surfaces through ExecutionFailed.
        server.Offline = false;
        await shown.Model.AddColumnAsync(Support.Choice(shown.Model, "status"));
        Assert.Equal(string.Empty, shown.Model.Status);
        server.Offline = true;
        await Pumping.WaitAsync(() => shown.Find<Button>("PublishButton").Command!.CanExecute(null));
        Clicks.On(shown.Find<Button>("PublishButton"));
        Assert.True(await Clicks.StatusContainsAsync(status, "That did not work"));

        // And for Run, which reads the server (a good op first clears the status line).
        server.Offline = false;
        await shown.Model.SetLabelAsync("Sales");
        Assert.Equal(string.Empty, shown.Model.Status);
        server.Offline = true;
        Clicks.On(shown.Find<Button>("RunButton"));
        Assert.True(await Clicks.StatusContainsAsync(status, "That did not work"));
    }

    [AvaloniaFact]
    public async Task A_masked_column_shows_the_mask_in_the_preview_and_the_run_and_never_a_value()
    {
        var server = new FakeBuilderServer { LeakMaskedValues = true };
        server.MaskedFields.Add("status");
        var shown = await Views.ReportAsync(server);
        var model = shown.Model;
        await model.AddColumnAsync(Support.Choice(model, "status"));
        await model.AddColumnAsync(Support.Choice(model, "grand_total"));
        await model.RunAsync();

        var preview = shown.Find<Spotledger.Designer.Reports.Views.RowsTableView>("PreviewTable");
        var run = shown.Find<Spotledger.Designer.Reports.Views.RowsTableView>("RunTable");
        foreach (var table in new[] { preview, run })
        {
            Assert.True(await Pumping.WaitAsync(() => table.GetVisualDescendantsOfText().Any(t => t == RowsView.MaskText)));
            Assert.DoesNotContain(table.GetVisualDescendantsOfText(), t => t.StartsWith("status-", StringComparison.Ordinal));
            Assert.Contains(table.GetVisualDescendantsOfText(), t => t.StartsWith("grand_total-", StringComparison.Ordinal));
        }
    }

    [AvaloniaFact]
    public async Task The_report_builder_renders_a_real_picture()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.ReportAsync(server);
        await shown.Model.AddColumnAsync(Support.Choice(shown.Model, "status"));
        await shown.Model.AddColumnAsync(Support.Choice(shown.Model, "grand_total"));
        await Pumping.WaitAsync(() => shown.Model.Preview.Rows.Count > 0);

        Assert.True(Views.DistinctColours(shown.Window, "report-builder") > 40, "the report builder frame looks blank");
    }
}

internal static class VisualText
{
    public static IEnumerable<string> GetVisualDescendantsOfText(this Avalonia.Controls.Control root) =>
        Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(root).OfType<TextBlock>().Select(t => t.Text ?? string.Empty);
}
