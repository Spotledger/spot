using Spotledger.Client.Expressions;
using Xunit;

namespace Spotledger.Client.Browser.Tests;

/// <summary>
/// Calculation cases beyond the corpus: the shapes the entry forms preview (line amount, tax, document
/// total, caps). The expected values were computed independently with Python's decimal module
/// (half-even, the rules of docs/expression-language.md section 6.3), not by this code. Every case runs on
/// the managed backend and on the wire path, and both must agree.
/// </summary>
public class CalculationCasesTests
{
    private static readonly ExpressionDateTime Now = ExpressionDateTime.Parse("2026-10-05T10:20:30Z")!.Value;

    private static Dictionary<string, ExpressionValue> Fields(params (string Path, ExpressionValue Value)[] fields) =>
        fields.ToDictionary(f => f.Path, f => f.Value, StringComparer.Ordinal);

    private static DecimalValue Dec(long mantissa, int scale) => new(mantissa, scale);

    public static TheoryData<string, string, string> Cases() => new()
    {
        // 3 x 19.99 less 12.5 % = 59.97 * 0.875000 = 52.473750, rounded to 2 places: 52.47
        { "line-amount", "round(doc.qty * doc.rate * (1 - doc.discount / 100), 2)", "52.47" },
        // 2.5 * 0.01 = 0.025: the tie goes to the even neighbour 0.02 (scale of the widest operand)
        { "tie-down", "doc.a * doc.b", "0.02" },
        // 3.5 * 0.01 = 0.035: the tie goes to the even neighbour 0.04
        { "tie-up", "doc.c * doc.b", "0.04" },
        // 1234.56 + round(1234.56 * 18 / 100, 2) = 1234.56 + 222.22
        { "document-total", "doc.net + round(doc.net * 18 / 100, 2)", "1456.78" },
        // 61.25 * 0.1 = 6.1250 -> 6.12 at scale 2 (tie to even); capped at 5.00 by min, which keeps its scale
        { "capped-fee", "min(round(doc.fee_base * 0.1, 2), 5.00)", "5.00" },
        // an absent field reads as null and the sum of null is null
        { "null-total", "doc.missing * 2", "null" },
        // coalesce picks the int 0 (the field has no value)
        { "default-discount", "coalesce(doc.missing, 0)", "0" },
    };

    private static CalcRequest Request(string source) => new(
        source,
        Now,
        Fields(
            ("doc.qty", new IntValue(3)),
            ("doc.rate", Dec(1999, 2)),
            ("doc.discount", Dec(125, 1)),
            ("doc.a", Dec(25, 1)),
            ("doc.b", Dec(1, 2)),
            ("doc.c", Dec(35, 1)),
            ("doc.net", Dec(123456, 2)),
            ("doc.fee_base", Dec(6125, 2))));

    [Theory]
    [MemberData(nameof(Cases))]
    public void CaseGivesTheIndependentResult(string name, string source, string expected)
    {
        var request = Request(source);
        var managed = new ManagedCalculationBackend().Evaluate(request);
        var path = new CalculationPath();
        path.UseModule(new InProcessModule());
        var wire = path.Evaluate(request);
        Assert.True(managed.IsOk, $"{name}: {managed}");
        Assert.Equal(expected, managed.Value!.ToString());
        Assert.True(managed.Same(wire), $"{name}: managed {managed}, wire {wire}");
    }

    [Fact]
    public void DivisionByZeroIsAnErrorCodeOnBothPaths()
    {
        var request = Request("doc.net / 0");
        var path = new CalculationPath();
        path.UseModule(new InProcessModule());
        Assert.Equal("expr.division_by_zero", new ManagedCalculationBackend().Evaluate(request).ErrorCode);
        Assert.True(path.Evaluate(request).Same(new ManagedCalculationBackend().Evaluate(request)));
    }

    [Fact]
    public void SyntaxErrorKeepsItsOffset()
    {
        var request = Request("1 +");
        var managed = new ManagedCalculationBackend().Evaluate(request);
        Assert.False(managed.IsOk);
        Assert.NotNull(managed.Offset);
        var path = new CalculationPath();
        path.UseModule(new InProcessModule());
        Assert.True(path.Evaluate(request).Same(managed));
    }

    [Fact]
    public void MoneyResultsAreExactDecimalsAtTheWidestScale()
    {
        var outcome = new ManagedCalculationBackend().Evaluate(Request("doc.net * 0.18"));
        Assert.IsType<DecimalValue>(outcome.Value);
        Assert.Equal("222.22", outcome.Value!.ToString());
    }
}
