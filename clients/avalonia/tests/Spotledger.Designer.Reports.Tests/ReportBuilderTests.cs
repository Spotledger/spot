using Spotledger.Designer.Reports;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

public sealed class ReportBuilderTests
{
    private static async Task<(FakeBuilderServer Server, ReportBuilderViewModel Builder)> OpenAsync(int previewRows = 50)
    {
        var server = new FakeBuilderServer();
        var builder = new ReportBuilderViewModel(server, "rpt-1", previewRows);
        await builder.OpenAsync();
        return (server, builder);
    }

    [Fact]
    public async Task A_grouped_report_sends_exactly_the_golden_op_sequence()
    {
        var (server, builder) = await OpenAsync();

        Assert.True(await builder.AddColumnAsync(Support.Choice(builder, "posting_date")));
        Assert.True(await builder.AddColumnAsync(Support.Choice(builder, "customer.territory")));
        Assert.True(await builder.AddColumnAsync(Support.Choice(builder, "grand_total"), aggregate: Aggregate.Sum));
        Assert.True(await builder.ToggleGroupAsync("posting_date"));
        Assert.True(await builder.ToggleGroupAsync("customer.territory"));
        Assert.True(await builder.SortByAsync("posting_date", SortDirection.Asc));
        Assert.True(await builder.SetTotalsAsync(["grand_total"]));
        Assert.True(await builder.SetChartAsync(new ChartSpec(ChartKind.Bar, "posting_date", ["grand_total"])));

        Assert.Equal(
            [
                """{"op":"add_column","field":"posting_date"}""",
                """{"op":"add_join","alias":"customer","doctype":"Customer","on":"customer"}""",
                """{"op":"add_column","field":"customer.territory"}""",
                """{"op":"add_column","field":"grand_total","aggregate":"sum"}""",
                """{"op":"group_by","field":"posting_date"}""",
                """{"op":"group_by","field":"customer.territory"}""",
                """{"op":"sort_by","field":"posting_date","direction":"asc"}""",
                """{"op":"set_totals","columns":["grand_total"]}""",
                """{"op":"set_chart","chart":{"kind":"bar","x":"posting_date","series":[{"column":"grand_total"}]}}""",
            ],
            server.ReportOps);
        Assert.Empty(builder.Issues);
        Assert.True(builder.CanPublish);
        Assert.Equal(3, builder.Preview.Columns.Count);
    }

    [Fact]
    public async Task Link_path_fields_read_as_a_person_would_say_them_and_the_link_is_declared_once()
    {
        var (server, builder) = await OpenAsync();

        var territory = Support.Choice(builder, "customer.territory");
        Assert.Equal("Customer > Territory", territory.DisplayName);

        await builder.AddColumnAsync(territory);
        await builder.AddColumnAsync(territory with { Path = "customer.territory" });

        Assert.Single(builder.Draft!.Joins);
        Assert.Equal(1, server.ReportOps.Count(o => o.Contains("add_join", StringComparison.Ordinal)));
    }

    [Fact]
    public async Task A_refused_op_leaves_the_draft_alone_and_says_why_in_plain_words()
    {
        var (_, builder) = await OpenAsync();
        var before = builder.Draft;
        var secret = new FieldChoice("salary", "Salary", new FieldOffer("salary", "Salary", OfferKind.Exact, false, true), null);

        Assert.False(await builder.AddColumnAsync(secret));

        Assert.Same(before, builder.Draft);
        Assert.Contains("no field called salary", builder.Status, StringComparison.Ordinal);
        Assert.Contains("Pick a field", builder.Status, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Typed_filter_editors_refuse_text_that_cannot_be_the_fields_type_without_calling_the_server()
    {
        var (server, builder) = await OpenAsync();

        var whole = ReportBuilderViewModel.NewFilter(Support.Choice(builder, "qty"));
        whole.ValueText = "twelve";
        Assert.False(await builder.AddFilterAsync(whole));
        Assert.Equal("Type a whole number, like 12.", builder.Status);

        var exact = ReportBuilderViewModel.NewFilter(Support.Choice(builder, "grand_total"));
        exact.ValueText = "1e3";
        Assert.False(await builder.AddFilterAsync(exact));

        var date = ReportBuilderViewModel.NewFilter(Support.Choice(builder, "posting_date"));
        date.ValueText = "31/03/2026";
        Assert.False(await builder.AddFilterAsync(date));

        Assert.Empty(server.ReportOps);
    }

    [Fact]
    public async Task Each_field_type_offers_only_the_comparisons_that_make_sense()
    {
        var (_, builder) = await OpenAsync();

        Assert.Equal(6, ReportBuilderViewModel.NewFilter(Support.Choice(builder, "grand_total")).Operators.Count);
        Assert.Equal([Compare.Eq, Compare.Ne], ReportBuilderViewModel.NewFilter(Support.Choice(builder, "status")).Operators);
        var yesNo = ReportBuilderViewModel.NewFilter(Support.Choice(builder, "is_return"));
        Assert.Equal([Compare.Eq], yesNo.Operators);
        Assert.Equal(EditorKind.YesNo, yesNo.Editor);
        Assert.False(yesNo.CanBeParameter);
        Assert.Throws<ArgumentOutOfRangeException>(() => yesNo.Operator = Compare.Lt);
    }

    [Fact]
    public async Task An_exact_decimal_filter_keeps_the_text_and_never_becomes_a_float()
    {
        var (server, builder) = await OpenAsync();

        var editor = ReportBuilderViewModel.NewFilter(Support.Choice(builder, "grand_total"));
        editor.Operator = Compare.Ge;
        editor.ValueText = "100.50";
        Assert.True(await builder.AddFilterAsync(editor));

        Assert.Equal(
            ["""{"op":"add_filter","filter":{"field":"grand_total","op":"ge","target":{"kind":"value","value":{"kind":"decimal","text":"100.50"}}}}"""],
            server.ReportOps);
    }

    [Fact]
    public async Task The_parameter_toggle_adds_the_parameter_before_the_filter_that_uses_it()
    {
        var (server, builder) = await OpenAsync();

        var editor = ReportBuilderViewModel.NewFilter(Support.Choice(builder, "posting_date"));
        editor.Operator = Compare.Ge;
        editor.IsParameter = true;
        Assert.True(await builder.AddFilterAsync(editor));

        Assert.Equal(
            [
                """{"op":"add_parameter","parameter":{"name":"posting_date","label":"Posting date","ty":"date","required":false}}""",
                """{"op":"add_filter","filter":{"field":"posting_date","op":"ge","target":{"kind":"parameter","name":"posting_date"}}}""",
            ],
            server.ReportOps);
    }

    [Fact]
    public async Task Undo_and_redo_follow_the_servers_op_log()
    {
        var (_, builder) = await OpenAsync();
        await builder.AddColumnAsync(Support.Choice(builder, "status"));
        await builder.AddColumnAsync(Support.Choice(builder, "qty"));
        Assert.Equal(2, builder.Draft!.Columns.Count);

        await Support.RunAsync(builder.UndoCommand);
        Assert.Single(builder.Draft!.Columns);

        await Support.RunAsync(builder.RedoCommand);
        Assert.Equal(2, builder.Draft!.Columns.Count);
    }

    [Fact]
    public async Task The_preview_never_shows_more_rows_than_the_cap_and_says_so()
    {
        var (server, builder) = await OpenAsync(previewRows: 25);
        await builder.AddColumnAsync(Support.Choice(builder, "status"));

        Assert.Equal(25, server.LastPreviewCap);
        Assert.Equal(25, builder.Preview.Rows.Count);
        Assert.Equal("Showing the first 25 rows.", builder.Preview.Note);
    }

    [Fact]
    public async Task A_masked_column_shows_the_mask_even_if_the_server_leaked_a_value()
    {
        var (server, builder) = await OpenAsync();
        server.MaskedFields.Add("grand_total");
        server.LeakMaskedValues = true;
        await builder.AddColumnAsync(Support.Choice(builder, "status"));
        await builder.AddColumnAsync(Support.Choice(builder, "grand_total"));

        Assert.True(builder.Preview.HasMaskedColumns);
        Assert.All(builder.Preview.Rows, row =>
        {
            Assert.StartsWith("status-", row.Cells[0], StringComparison.Ordinal);
            Assert.Equal(RowsView.MaskText, row.Cells[1]);
        });

        await builder.RunAsync();
        Assert.All(builder.RunResult.Rows, row => Assert.Equal(RowsView.MaskText, row.Cells[1]));
    }

    [Fact]
    public async Task Save_publish_and_share_go_through_the_server()
    {
        var (server, builder) = await OpenAsync();
        await builder.AddColumnAsync(Support.Choice(builder, "status"));

        await Support.RunAsync(builder.SaveCommand);
        Assert.True(server.Saved);

        Assert.True(await builder.ShareAsync(["Accounts User"]));
        Assert.Equal("""{"op":"set_roles","roles":["Accounts User"]}""", server.ReportOps[^1]);

        await Support.RunAsync(builder.PublishCommand);
        Assert.Equal("Published sales_report.", builder.PublishMessage);
    }

    [Fact]
    public async Task Publish_stays_disabled_while_the_server_reports_a_blocking_issue()
    {
        var (_, builder) = await OpenAsync();
        await builder.AddColumnAsync(Support.Choice(builder, "status"));
        await builder.AddColumnAsync(Support.Choice(builder, "qty"));
        await builder.ToggleGroupAsync("status");

        Assert.Contains(builder.Issues, i => i.Message.Contains("neither grouped nor summed", StringComparison.Ordinal));
        Assert.False(builder.PublishCommand.CanExecute(null));

        await builder.SetAggregateAsync("qty", Aggregate.Sum);
        Assert.True(builder.PublishCommand.CanExecute(null));
    }

    [Fact]
    public void The_exact_decimal_check_matches_the_servers_rule()
    {
        Assert.True(FilterEditor.IsExactDecimal("12"));
        Assert.True(FilterEditor.IsExactDecimal("-0.5"));
        Assert.False(FilterEditor.IsExactDecimal("1."));
        Assert.False(FilterEditor.IsExactDecimal(".5"));
        Assert.False(FilterEditor.IsExactDecimal("1,5"));
        Assert.False(FilterEditor.IsExactDecimal("1e3"));
        Assert.False(FilterEditor.IsExactDecimal(string.Empty));
    }
}
