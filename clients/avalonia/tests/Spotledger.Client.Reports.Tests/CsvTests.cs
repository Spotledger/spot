using System.Text;
using Spotledger.Client.Reports;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

public sealed class CsvTests
{
    private static readonly ReportColumn[] Columns =
    [
        new("memo", "Memo", "text"),
        new("party", "Party", "link"),
        new("balance", "Balance", "decimal"),
        new("count", "Count", "int"),
    ];

    private static readonly string?[][] Rows =
    [
        ["=SUM(A1:A9)", "C1", "10.50", "3"],
        ["+cmd|' /C calc'!A0", "C2", "-5.25", "-1"],
        ["-2+3", "C3", "=1+1", "7"],
        ["@SUM(1)", "C,4", "0.00", "0"],
        ["\tleading tab", "C5", "1", "1"],
        ["\rleading cr", "C6", "2", "2"],
        ["say \"hi\", ok", "multi\nline", "2.5", "2"],
        [null, null, null, null],
        ["مرحبا", "ج", "1000000.000001", "1"],
    ];

    private static string GoldenText(string name) => File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "Golden", name), Encoding.UTF8);

    [Fact]
    public async Task TheCsvOfAHostileResultIsTheGoldenFileByteForByte()
    {
        using var stream = new MemoryStream();

        var written = await ReportCsv.WriteAsync(stream, Columns, Rows, TestContext.Current.CancellationToken);

        Assert.Equal(9, written);
        var bytes = stream.ToArray();
        Assert.Equal(File.ReadAllBytes(Path.Combine(AppContext.BaseDirectory, "Golden", "export-hardening.csv")), bytes);
        Assert.False(bytes.Length >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF, "no byte-order mark");
    }

    [Fact]
    public void TheTsvOfTheSelectionIsTheGoldenFileAndNeverStartsAFormula()
    {
        var tsv = ReportCsv.ToTsv(Columns, Rows, includeHeader: true);

        Assert.Equal(GoldenText("export-hardening.tsv"), tsv);
    }

    [Theory]
    [InlineData("=1+1", "'=1+1")]
    [InlineData("+1", "'+1")]
    [InlineData("-1", "'-1")]
    [InlineData("@a", "'@a")]
    [InlineData("\tx", "'\tx")]
    [InlineData("\rx", "'\rx")]
    [InlineData("plain", "plain")]
    [InlineData("a=b", "a=b")]
    [InlineData("", "")]
    public void OnlyALeadingFormulaCharacterIsPrefixed(string input, string expected)
        => Assert.Equal(expected, ReportCsv.Harden(input));

    [Theory]
    [InlineData("-5.25", true, "-5.25")]
    [InlineData("+5", true, "+5")]
    [InlineData("-5.25", false, "'-5.25")]
    [InlineData("=SUM(A1)", true, "'=SUM(A1)")]
    [InlineData("1e9", true, "1e9")]
    public void ANumberColumnKeepsRealNegativesButHardensEverythingThatIsNotANumber(string input, bool numeric, string expected)
        => Assert.Equal(expected, ReportCsv.Cell(input, numeric));

    [Fact]
    public async Task ALargeResultIsWrittenARowAtATimeAndStopsWhenCancelled()
    {
        using var cts = new CancellationTokenSource();
        var produced = 0;
        IEnumerable<string?[]> Many()
        {
            for (var i = 0; i < 1_000_000; i++)
            {
                produced++;
                if (i == 2000)
                {
                    cts.Cancel();
                }

                yield return ["row " + i, "p", "1.00", "1"];
            }
        }

        using var stream = new MemoryStream();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => ReportCsv.WriteAsync(stream, Columns, Many(), cts.Token));

        Assert.True(produced < 3000, "the writer pulled rows one at a time and stopped at the cancel");
        Assert.True(stream.Length > 0, "rows already produced were flushed to the destination, not held back");
    }

    [Fact]
    public async Task AStreamIsLeftOpenForTheCaller()
    {
        using var stream = new MemoryStream();

        await ReportCsv.WriteAsync(stream, Columns, [], TestContext.Current.CancellationToken);

        Assert.True(stream.CanWrite);
        Assert.Equal("Memo,Party,Balance,Count\r\n", Encoding.UTF8.GetString(stream.ToArray()));
    }
}
