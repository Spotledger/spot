using System.Text;
using System.Text.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Spotledger.Client.Reports;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

public sealed class FormAndJsonTests
{
    private static string Golden(string name) => File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "Golden", name)).Replace("\r\n", "\n", StringComparison.Ordinal);

    private static string Describe(ReportParameters form)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream, new JsonWriterOptions { Indented = true }))
        {
            writer.WriteStartArray();
            foreach (var p in form.Items)
            {
                writer.WriteStartObject();
                writer.WriteString("name", p.Name);
                writer.WriteString("label", p.Field.Label);
                writer.WriteString("kind", p.Field.Kind.ToString());
                writer.WriteBoolean("required", p.Field.Required);
                if (p.Field.Scale is { } scale)
                {
                    writer.WriteNumber("scale", scale);
                }

                if (p.Field.Precision is { } precision)
                {
                    writer.WriteNumber("precision", precision);
                }

                if (p.Field.Options.Count > 0)
                {
                    writer.WriteString("options", string.Join("|", p.Field.Options));
                }

                if (p.Field.LinkTo is { } link)
                {
                    writer.WriteString("linkTo", link);
                }

                writer.WriteString("default", p.Text);
                writer.WriteEndObject();
            }

            writer.WriteEndArray();
        }

        return Encoding.UTF8.GetString(stream.ToArray()).Replace("\r\n", "\n", StringComparison.Ordinal) + "\n";
    }

    [Fact]
    public void TheDescriptionOfEveryParameterTypeBecomesTheGoldenForm()
    {
        var description = ReportJson.ReadDescription(Fx.AllTypesDescription);

        var form = new ReportParameters(description.Parameters);

        Assert.Equal(Golden("form-all-types.json"), Describe(form));
    }

    [Fact]
    public void ADescriptionKeepsItsColumnsAndTheSummableFlag()
    {
        var description = ReportJson.ReadDescription(Fx.AllTypesDescription);

        Assert.Equal(["account", "posted", "memo", "balance"], description.Columns.Select(c => c.Name));
        Assert.Equal([false, false, false, true], description.Columns.Select(c => c.Summable));
        Assert.Equal(50000, description.MaxRows);
    }

    [Fact]
    public void ARunResultKeepsEveryDigitOfADecimalWhetherItArrivesAsAStringOrANumberToken()
    {
        const string json = """
            {"data":{"columns":[{"name":"a","label":"A","type":"decimal"},{"name":"b","label":"B","type":"text"}],
             "rows":[["12345678901234567890.123456789","x"],[98765432109876543210987654321.00,null],[true,"y"]],"truncated":true},
             "messages":[{"level":"info","code":"report.columns_removed","message":"2 columns were removed for you."}]}
            """;

        var run = ReportJson.ReadRun(json);

        Assert.True(run.Truncated);
        Assert.Equal("12345678901234567890.123456789", run.Rows[0][0]);
        Assert.Equal("98765432109876543210987654321.00", run.Rows[1][0]);
        Assert.Null(run.Rows[1][1]);
        Assert.Equal("true", run.Rows[2][0]);
        Assert.Equal("report.columns_removed", Assert.Single(run.Messages).Code);
    }

    [Fact]
    public void ARowShorterOrLongerThanTheColumnsIsPaddedOrCut()
    {
        const string json = """{"data":{"columns":[{"name":"a","label":"A","type":"text"},{"name":"b","label":"B","type":"text"}],"rows":[["1"],["1","2","3"]],"truncated":false}}""";

        var run = ReportJson.ReadRun(json);

        Assert.Equal<IEnumerable<string?>>(["1", null], run.Rows[0]);
        Assert.Equal<IEnumerable<string?>>(["1", "2"], run.Rows[1]);
    }

    [Fact]
    public void AnUnknownParameterTypeMakesTheReportUnrunnable()
    {
        var description = ReportJson.ReadDescription("""{"data":{"name":"r","label":"R","app":"a","base_doctype":"d","max_rows":10,"parameters":[{"name":"x","label":"X","type":"geo","required":false}],"columns":[]}}""");

        Assert.True(new ReportParameters(description.Parameters).HasUnsupported);
    }

    [Fact]
    public void ADecimalParameterAcceptsExactTextAndRefusesMorePlacesThanItsScaleWithoutRounding()
    {
        var form = new ReportParameters([new ReportParameterDef("amount", "Amount", ReportParameterType.DecimalNumber, false, null, null)]);
        var amount = form.Items[0];

        Assert.True(amount.TrySetText("1,234.567891"));
        Assert.Equal("1234.567891", amount.Text);
        Assert.False(amount.TrySetText("1.1234567"));
        Assert.Contains("decimal place", amount.Error, StringComparison.Ordinal);
        Assert.False(amount.TrySetText("abc"));
        Assert.NotNull(amount.Error);
    }

    [Fact]
    public void ARequiredParameterWithoutAValueIsRefusedLocallyAndAllErrorsAreMarkedAtOnce()
    {
        var form = new ReportParameters(
        [
            new ReportParameterDef("a", "Alpha", ReportParameterType.Text, true, null, null),
            new ReportParameterDef("b", "Beta", ReportParameterType.IntegerNumber, true, null, null),
        ]);
        form.Items[1].TrySetText("x1");

        Assert.False(form.Validate());

        Assert.Equal("Alpha is required.", form.Items[0].Error);
        Assert.NotNull(form.Items[1].Error);
    }

    [Fact]
    public void FiltersAreWrittenInTheContractEncodingAndEmptyParametersAreOmitted()
    {
        var description = ReportJson.ReadDescription(Fx.AllTypesDescription);
        var form = new ReportParameters(description.Parameters);
        form.Find("customer")!.TrySetText("CUST-0001");

        Assert.Equal("""{"customer":"CUST-0001","from_date":"2026-01-01","min_amount":"100.50","min_lines":3,"status":"Open"}""", form.ToFiltersJson());
    }

    [Fact]
    public void AServerRefusalLandsOnTheParameterItNames()
    {
        var form = new ReportParameters(ReportJson.ReadDescription(Fx.AllTypesDescription).Parameters);

        var byField = form.MapServerError(Fx.Error("report.bad_argument", "A report argument does not fit.", 422, new ApiFieldError("filters.min_lines", "report.bad_argument", "min_lines must be a whole number.")));
        var byMessage = form.MapServerError(Fx.Error("report.bad_argument", "The argument from_date does not fit its type."));
        var nowhere = form.MapServerError(Fx.Error("report.bad_argument", "Something is wrong."));

        Assert.True(byField);
        Assert.Equal("min_lines must be a whole number.", form.Find("min_lines")!.Error);
        Assert.True(byMessage);
        Assert.Contains("from_date", form.Find("from_date")!.Error, StringComparison.Ordinal);
        Assert.False(nowhere);
    }

    [Fact]
    public void TheDigestIsStableAndDependsOnlyOnTheValuesThatAreSet()
    {
        var a = ReportParameters.DigestOf(new Dictionary<string, string> { ["x"] = "1", ["y"] = "2" });
        var b = ReportParameters.DigestOf(new Dictionary<string, string> { ["y"] = "2", ["x"] = "1", ["z"] = string.Empty });
        var c = ReportParameters.DigestOf(new Dictionary<string, string> { ["x"] = "1", ["y"] = "3" });

        Assert.Equal(a, b);
        Assert.NotEqual(a, c);
        Assert.Equal(16, a.Length);
        Assert.Equal(ReportParameters.DigestOf(null), ReportParameters.DigestOf(new Dictionary<string, string>()));
    }
}

public sealed class ExactTotalsTests
{
    private static ReportResultSource Source(params string?[] balances)
        => new([new ReportColumn("memo", "Memo", "text"), new ReportColumn("balance", "Balance", "decimal", Summable: true)],
            balances.Select((b, i) => new string?[] { "r" + i, b }).ToArray());

    [Fact]
    public void TwentyNineDigitValuesAddUpExactly()
    {
        var source = Source("12345678901234567890123456789.01", "0.99", "-1.00");

        Assert.Equal("12345678901234567890123456789.00", source.TotalOf(1));
    }

    [Fact]
    public void NegativesCentsAndMixedScalesAddWithoutDrift()
    {
        var source = Source("0.10", "0.20", "-0.30", "1.005", "-1.005", "7");

        Assert.Equal("7.000", source.TotalOf(1));
    }

    [Fact]
    public void ThousandsOfCentsDoNotDriftLikeFloatingPointWould()
    {
        var source = Source(Enumerable.Repeat("0.10", 10_000).ToArray());

        Assert.Equal("1000.00", source.TotalOf(1));
    }

    [Fact]
    public void AnEmptyCellIsSkippedAndANonNumberMakesTheTotalUnavailable()
    {
        Assert.Equal("3.50", Source("1.50", null, "2.00").TotalOf(1));
        Assert.Equal(string.Empty, Source("1.50", "n/a").TotalOf(1));
    }

    [Fact]
    public void ANegativeSumIsWrittenWithItsSignAndAZeroSumWithoutOne()
    {
        Assert.Equal("-0.05", Source("0.10", "-0.15").TotalOf(1));
        Assert.Equal("0.00", Source("0.10", "-0.10").TotalOf(1));
    }

    [Fact]
    public void TheFooterLabelsTheTotalAsLoadedRowsAndOnlyDecimalSummableColumnsGetOne()
    {
        var footer = Source("1.00", "2.00").BuildFooter()!;

        Assert.Equal(ReportResultSource.TotalLabel, footer.Cells[0].Text);
        Assert.Equal("3.00", footer.Cells[1].Text);
        Assert.Equal(Spotledger.Client.Grid.GridCellAlignment.End, footer.Cells[1].Alignment);
    }

    [Fact]
    public void ACutResultHasNoTotalsRowAndNeitherDoesAColumnTheReportDoesNotCallSummable()
    {
        var rows = new[] { new string?[] { "a", "1.00" } };
        var cut = new ReportResultSource([new ReportColumn("m", "M", "text"), new ReportColumn("b", "B", "decimal", Summable: true)], rows, truncated: true);
        var plain = new ReportResultSource([new ReportColumn("m", "M", "text"), new ReportColumn("b", "B", "decimal")], rows);

        Assert.Null(cut.BuildFooter());
        Assert.Null(plain.BuildFooter());
    }

    [Fact]
    public async Task SortingADecimalColumnComparesValuesNotTextAndKeepsEmptyCellsLast()
    {
        var source = Source("9.5", "10.25", "-3", null, "10.250");
        await source.Sort(1, Spotledger.Client.Grid.GridSortDirection.Ascending);

        var asc = (await source.GetWindow(0, 10, CancellationToken.None)).Select(r => r.Cells[1].Text).ToArray();
        await source.Sort(1, Spotledger.Client.Grid.GridSortDirection.Descending);
        var desc = (await source.GetWindow(0, 10, CancellationToken.None)).Select(r => r.Cells[1].Text).ToArray();

        Assert.Equal(["-3", "9.5", "10.25", "10.250", string.Empty], asc);
        Assert.Equal(["10.25", "10.250", "9.5", "-3", string.Empty], desc);
    }

    [Fact]
    public async Task TheQuickFilterNarrowsTheShownRowsAndAColumnMoveReordersTheCells()
    {
        var source = Source("1.00", "2.00", "30.00");

        source.SetFilter("r1");
        var one = await source.GetWindow(0, 10, CancellationToken.None);
        source.SetFilter(null);
        source.MoveColumn(0, 1);
        var moved = await source.GetWindow(0, 1, CancellationToken.None);

        Assert.Equal("r1", Assert.Single(one).Cells[0].Text);
        Assert.Equal(["Balance", "Memo"], source.Columns.Select(c => c.Label));
        Assert.Equal(["1.00", "r0"], moved[0].Cells.Select(c => c.Text));
    }

    [Fact]
    public void ANumberColumnIsWideEnoughForItsLongestExactValueSoNoLeadingDigitIsClipped()
    {
        var source = Source("12345678901234567890123456789.01", "1.00");

        var widths = source.BuildGridColumns().Select(c => c.Width).ToArray();

        Assert.True(widths[1].Value >= 32 * 8, $"width {widths[1].Value} fits 32 characters");
        Assert.False(widths[1].IsStar);
        Assert.True(widths[0].IsStar);
    }
}
