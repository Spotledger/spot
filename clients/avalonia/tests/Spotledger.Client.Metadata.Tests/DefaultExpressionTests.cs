using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 4: default expressions. They run once, on creation, in dependency order
/// (the declaration order in <see cref="RuleTestModels.Defaults"/> is deliberately
/// wrong), never again; decimals stay exact and a value of the wrong type leaves the
/// static default in place with a diagnostic.
/// </summary>
public sealed class DefaultExpressionTests
{
    private static readonly DateTimeOffset Clock = new(2026, 10, 21, 9, 30, 0, TimeSpan.Zero);

    [Fact]
    public void DefaultsAreEvaluatedOnceInDependencyOrder()
    {
        var draft = new DocumentDraft(RuleTestModels.Defaults(), timeProvider: new RuleTestModels.FixedTime(Clock));

        Assert.Equal("100.00", Decimal(draft, "base"));
        Assert.Equal("102.50", Decimal(draft, "tax"));
        // total reads base and tax: in declaration order it would have read two nulls.
        Assert.Equal("202.50", Decimal(draft, "total"));
        Assert.Equal("generated", Assert.IsType<TextFieldValue>(draft.Get("label")).Value);
        Assert.True(Assert.IsType<BoolFieldValue>(draft.Get("flag")).Value);
        Assert.Equal(Clock, Assert.IsType<DateTimeFieldValue>(draft.Get("stamp")).Value);
        Assert.False(draft.IsDirty);
    }

    [Fact]
    public void DefaultsStayExact()
    {
        var model = new DocTypeModel(
            "exact",
            "Exact",
            1,
            false,
            [
                new FieldModel("sum", "Sum", FieldKind.DecimalNumber, scale: 2, precision: 18,
                    defaultExpr: "0.10 + 0.20"),
            ],
            exprVersion: RuleTestModels.Version);
        var draft = new DocumentDraft(model);

        // A double would give 0.30000000000000004 here.
        Assert.Equal("0.30", Decimal(draft, "sum"));
    }

    [Fact]
    public void ADefaultOfTheWrongTypeKeepsTheStaticDefaultAndIsReported()
    {
        var model = new DocTypeModel(
            "mistyped",
            "Mistyped",
            1,
            false,
            [
                new FieldModel("label", "Label", FieldKind.Text, defaultValue: new TextFieldValue("static"),
                    defaultExpr: "42"),
            ],
            exprVersion: RuleTestModels.Version);
        var draft = new DocumentDraft(model);

        Assert.Equal("static", Assert.IsType<TextFieldValue>(draft.Get("label")).Value);
        var diagnostic = Assert.Single(draft.Rules.Diagnostics);
        Assert.Equal(RuleDiagnosticCode.DefaultType, diagnostic.Code);
        Assert.Equal(RuleKind.Default, diagnostic.Kind);
    }

    [Fact]
    public void ADefaultIsNotReappliedAfterAnEditOrARevert()
    {
        var draft = new DocumentDraft(RuleTestModels.Defaults(), timeProvider: new RuleTestModels.FixedTime(Clock));
        var evaluations = draft.Rules.EvaluationCount;

        draft.Set("base", new DecimalFieldValue(ExactDecimal.FromCanonical("5.00")));
        Assert.Equal("5.00", Decimal(draft, "base"));

        // The default expressions are not rules: no change re-runs them, and a revert
        // goes back to the value creation produced, not to a fresh evaluation.
        draft.Revert();
        Assert.Equal("100.00", Decimal(draft, "base"));
        Assert.Equal("202.50", Decimal(draft, "total"));
        Assert.Equal(evaluations, draft.Rules.EvaluationCount);
    }

    [Fact]
    public void ANewChildRowGetsItsDefaultExpression()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        var row = draft.Table("items").AddRow();

        Assert.Equal("each", Assert.IsType<TextFieldValue>(row.GetValue("unit")).Value);
        // The row is a new row with its defaults as the values it started from.
        Assert.False(row.Values["unit"].IsDirty);
    }

    [Fact]
    public void AModelWithoutExpressionVersionStillAppliesStaticDefaults()
    {
        var model = new DocTypeModel(
            "plain",
            "Plain",
            1,
            false,
            [new FieldModel("label", "Label", FieldKind.Text, defaultValue: new TextFieldValue("static"),
                defaultExpr: "'ignored'")],
            exprVersion: RuleTestModels.Version + 1);
        var draft = new DocumentDraft(model);

        Assert.Equal("static", Assert.IsType<TextFieldValue>(draft.Get("label")).Value);
    }

    private static string Decimal(DocumentDraft draft, string field) =>
        Assert.IsType<DecimalFieldValue>(draft.Get(field)).CanonicalText;
}
