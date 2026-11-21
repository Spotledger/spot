using System;
using System.Collections.Generic;
using Spotledger.Client.Expressions;
using Xunit;

namespace Spotledger.Client.Expressions.Tests;

/// <summary>Behaviour tests beyond the corpus: the canonical printer (specification section 10),
/// the public API surface, and edges the corpus exercises only lightly. Nothing here weakens
/// the corpus; these are extra checks.</summary>
public class PrinterTests
{
    public static readonly TheoryData<string> Sources = new()
    {
        "1 + 2 * 3",
        "(1 + 2) * 3",
        "1 < 2",
        "doc.qty > 2 ? 'many' : 'few'",
        "not doc.ok and (doc.qty in [1, 2, 3]) or false",
        "1.50 + parent.total - 0.5",
        "-(1 + 2) * -3",
        "contains('abc', 'b')",
    };

    [Theory]
    [MemberData(nameof(Sources))]
    public void ParsesBackToAnEqualTree(string source)
    {
        ExpressionNode first = Expression.Parse(source);
        string printed = first.ToString();
        ExpressionNode second = Expression.Parse(printed);
        Assert.True(
            first.ToString() == second.ToString(),
            $"{source}: printed as {printed}, which parses to a different tree {second}");
    }

    [Theory]
    [InlineData("1 + 2 * 3", "1 + 2 * 3")]
    [InlineData("(1 + 2) * 3", "(1 + 2) * 3")]
    [InlineData("1 + (2 * 3)", "1 + 2 * 3")]
    [InlineData("(1 + 2) + 3", "1 + 2 + 3")]
    [InlineData("1 - (2 - 3)", "1 - (2 - 3)")]
    [InlineData("not (1 == 2)", "not 1 == 2")]
    [InlineData("(1 == 2) == true", "(1 == 2) == true")]
    [InlineData("1 == (2 == true)", "1 == (2 == true)")]
    [InlineData("(1 < 2) and true", "1 < 2 and true")]
    [InlineData("1 < 2 and true", "1 < 2 and true")]
    public void CanonicalForm(string source, string canonical)
    {
        Assert.Equal(canonical, Expression.Parse(source).ToString());
    }

    [Theory]
    [InlineData("'a\\nb'", "\"a\\nb\"")]
    [InlineData("\"it's\"", "\"it's\"")]
    [InlineData("'say \\\"hi\\\"'", "\"say \\\"hi\\\"\"")]
    [InlineData("'tab\\there'", "\"tab\\there\"")]
    public void QuotesTexts(string source, string canonical)
    {
        Assert.Equal(canonical, Expression.Parse(source).ToString());
    }

    [Fact]
    public void DecimalsKeepTheirScale()
    {
        Assert.Equal("1.50", Expression.Parse("1.50").ToString());
        Assert.Equal("0.0000000000000000000000000001", Expression.Parse("0.0000000000000000000000000001").ToString());
        Assert.Equal("12", Expression.Parse("12").ToString());
    }

    [Fact]
    public void ValueTextForms()
    {
        Assert.Equal("true", new BoolValue(true).ToString());
        Assert.True(new TextValue("[1, 2.50, \"x\"]")
            .Same(Expression.Evaluate("text([1, 2.50, \"x\"])", Ctx())));
        Assert.True(new TextValue("[]").Same(Expression.Evaluate("text([])", Ctx())));
        Assert.True(new TextValue("[1, \"a\", null, 2.50, [true]]")
            .Same(Expression.Evaluate("text([1, 'a', null, 2.50, [true]])", Ctx())));
        Assert.True(new TextValue("[\"a\\\"b\"]")
            .Same(Expression.Evaluate("text(['a\"b'])", Ctx())));
        Assert.Equal("-7", new IntValue(-7).ToString());
        Assert.Equal("-0.50", new DecimalValue(-50, 2).ToString());
    }

    [Fact]
    public void MapContextReadsDottedPaths()
    {
        ExpressionMapContext ctx = Ctx()
            .With("doc.qty", new IntValue(3));
        Assert.True(new IntValue(3).Same(Expression.Evaluate("doc.qty", ctx)));
        Assert.True(ExpressionValue.Null.Same(Expression.Evaluate("doc.other", ctx)));
        Assert.True(ExpressionValue.Null.Same(Expression.Evaluate("parent.qty", ctx)));
    }

    [Fact]
    public void ParseErrorsCarryCodesAndOffsets()
    {
        ExpressionParseException ex = Assert.Throws<ExpressionParseException>(() => Expression.Parse("1 +"));
        Assert.Equal(ExpressionErrorCode.Syntax, ex.Code);
        Assert.Equal(3, ex.Offset);
        ExpressionParseException bad = Assert.Throws<ExpressionParseException>(() => Expression.Parse("'x\\q'"));
        Assert.Equal(ExpressionErrorCode.BadEscape, bad.Code);
        Assert.Equal(2, bad.Offset);
        ExpressionParseException range = Assert.Throws<ExpressionParseException>(
            () => Expression.Parse("9223372036854775808"));
        Assert.Equal(ExpressionErrorCode.NumberRange, range.Code);
    }

    [Fact]
    public void IntArithmeticIsChecked()
    {
        IExpressionContext ctx = new ExpressionMapContext(Ctx().Now);
        ExpressionEvaluationException ex = Assert.Throws<ExpressionEvaluationException>(
            () => Expression.Evaluate("9223372036854775807 + 1", ctx));
        Assert.Equal(ExpressionErrorCode.Overflow, ex.Code);
        Assert.True(new IntValue(-9223372036854775807L - 1)
            .Same(Expression.Evaluate("-9223372036854775807 - 1", ctx)));
        Assert.True(new IntValue(1).Same(Expression.Evaluate("7 % 3", ctx)));
        Assert.True(new IntValue(-1).Same(Expression.Evaluate("-7 % 3", ctx)));
    }

    [Fact]
    public void DecimalDivisionHasAtLeastSixPlaces()
    {
        IExpressionContext ctx = new ExpressionMapContext(Ctx().Now);
        Assert.Equal("3.500000", Expression.Evaluate("35 / 10", ctx).ToString());
        Assert.Equal("0.333333", Expression.Evaluate("1 / 3", ctx).ToString());
        Assert.Equal("0.666667", Expression.Evaluate("2 / 3", ctx).ToString());
        Assert.Equal("7.500000", Expression.Evaluate("15 / 2", ctx).ToString());
        Assert.Equal("3.350000", Expression.Evaluate("35 / 10 - 0.15", ctx).ToString());
    }

    private static ExpressionMapContext Ctx()
    {
        return new ExpressionMapContext(ExpressionDateTime.Parse("2026-01-01T00:00:00Z")
            ?? throw new InvalidOperationException("the test instant must parse"));
    }

    private static (ExpressionMapContext, Action<string, string>) Evaluator()
    {
        ExpressionMapContext ctx = Ctx();
        return (ctx, (source, expected) =>
            Assert.Equal(expected, Expression.Evaluate(source, ctx).ToString()));
    }

    [Fact]
    public void DecimalRoundingMatchesTheRustEngine()
    {
        (ExpressionMapContext ctx, Action<string, string> check) = Evaluator();
        _ = ctx;
        // The rounding rule, once: half to even (specification section 6.3).
        check("round(0.125, 2)", "0.12");
        check("round(0.135, 2)", "0.14");
        check("round(-0.125, 2)", "-0.12");
        check("round(-0.135, 2)", "-0.14");
        check("round(5, 2)", "5.00");
        check("1.25 * 1.25", "1.56");
        check("1.5 * 1.5", "2.2");
        check("2.5 * 3", "7.5");
        check("1 / 3", "0.333333");
        check("2 / 3", "0.666667");
        check("1.0 / 4", "0.250000");
        check("-1 / 3", "-0.333333");
    }

    [Fact]
    public void DecimalOverflowMatchesTheRustEngine()
    {
        (ExpressionMapContext ctx, _) = Evaluator();
        // 10^28 has 29 significant digits: the decimal engine rejects it (the integer literal
        // form is a number_range parse error, as in the corpus).
        Assert.Throws<ExpressionParseException>(
            () => Expression.Evaluate("9999999999999999999999999999 + 1", ctx));
        ExpressionEvaluationException add = Assert.Throws<ExpressionEvaluationException>(
            () => Expression.Evaluate("decimal('9999999999999999999999999999') + 1", ctx));
        Assert.Equal(ExpressionErrorCode.Overflow, add.Code);
        ExpressionEvaluationException mul = Assert.Throws<ExpressionEvaluationException>(
            () => Expression.Evaluate("decimal('9999999999999999999999999999') * 10", ctx));
        Assert.Equal(ExpressionErrorCode.Overflow, mul.Code);
        // 2 * (5 * 10^27) = 10^28: 29 significant digits, overflow.
        ExpressionEvaluationException wide = Assert.Throws<ExpressionEvaluationException>(
            () => Expression.Evaluate("decimal('5.0000000000000000000000000000') * 2", ctx));
        Assert.Equal(ExpressionErrorCode.Overflow, wide.Code);
    }

    [Fact]
    public void NegMinIntGivesOverflow()
    {
        IExpressionContext ctx = new ExpressionMapContext(Ctx().Now);
        ExpressionEvaluationException ex = Assert.Throws<ExpressionEvaluationException>(
            () => Expression.Evaluate("-(-9223372036854775807 - 1)", ctx));
        Assert.Equal(ExpressionErrorCode.Overflow, ex.Code);
    }

    [Fact]
    public void TextEscapesRoundTripThroughLiterals()
    {
        ExpressionValue v = Expression.Evaluate("'a\\tb\\n'", new ExpressionMapContext(Ctx().Now));
        Assert.Equal("a\tb\n", Assert.IsType<TextValue>(v).Value);
    }

    [Fact]
    public void SameChecksScaleOnDecimals()
    {
        // Same: numerically equal AND the same scale; == compares the numbers alone.
        Assert.False(new DecimalValue(150, 2).Same(new DecimalValue(1500, 3)));
        Assert.True(new DecimalValue(150, 2).Same(new DecimalValue(150, 2)));
        Assert.False(new DecimalValue(150, 2).Same(new DecimalValue(15, 1)));
        Assert.False(new DecimalValue(150, 2).Same(new DecimalValue(1500, 2)));
    }
}