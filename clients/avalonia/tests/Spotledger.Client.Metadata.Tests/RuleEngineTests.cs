using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// The rule engine (deliverables 1, 2 and 3): each rule kind, a chain of rules, a cycle
/// detected at build time, only the affected rules re-run, a child row's rules reading
/// <c>parent</c>, exact decimals, a parse error isolated to its own rule, and the batched
/// change notification.
/// </summary>
public sealed class RuleEngineTests
{
    [Fact]
    public void DependsOnShowsAndHidesAField()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        Assert.False(draft.Fields["receipt_note"].IsVisible);

        draft.Set("has_receipt", new BoolFieldValue(true));
        Assert.True(draft.Fields["receipt_note"].IsVisible);

        draft.Set("has_receipt", new BoolFieldValue(false));
        Assert.False(draft.Fields["receipt_note"].IsVisible);
    }

    [Fact]
    public void MandatoryDependsOnAddsToAStaticRequired()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        Assert.False(draft.Fields["justification"].IsVisible);

        draft.Set("amount", Amount("50.00"));
        Assert.True(draft.Fields["justification"].IsVisible);
        Assert.False(draft.Fields["justification"].IsMandatory);

        draft.Set("amount", Amount("150.00"));
        Assert.True(draft.Fields["justification"].IsMandatory);
    }

    [Fact]
    public void ReadOnlyDependsOnAddsToAStaticReadOnlyAndTheStaticPropertyWins()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        Assert.False(draft.Fields["status"].IsReadOnly);

        // total is statically read-only: a rule that is false cannot make it editable.
        Assert.True(draft.Fields["total"].IsReadOnly);
        draft.Set("amount", Amount("10.00"));
        Assert.True(draft.Fields["total"].IsReadOnly);

        draft.Set("status", new SelectFieldValue("Paid"));
        Assert.True(draft.Fields["status"].IsReadOnly);
        draft.Set("status", new SelectFieldValue("Draft"));
        Assert.False(draft.Fields["status"].IsReadOnly);
    }

    [Fact]
    public void AChainOfRulesResolvesOneStepAtATime()
    {
        // has_receipt shows receipt_note, and a receipt note shows receipt_detail.
        var draft = new DocumentDraft(RuleTestModels.Claim());
        Assert.False(draft.Fields["receipt_note"].IsVisible);
        Assert.False(draft.Fields["receipt_detail"].IsVisible);

        draft.Set("has_receipt", new BoolFieldValue(true));
        Assert.True(draft.Fields["receipt_note"].IsVisible);
        Assert.False(draft.Fields["receipt_detail"].IsVisible);

        draft.Set("receipt_note", new TextFieldValue("attached"));
        Assert.True(draft.Fields["receipt_detail"].IsVisible);

        draft.Set("has_receipt", new BoolFieldValue(false));
        Assert.False(draft.Fields["receipt_note"].IsVisible);
        // The value is still there, so the rule that reads it still holds: hiding a field
        // never silently empties it (only the server decides what is stored).
        Assert.True(draft.Fields["receipt_detail"].IsVisible);
    }

    [Fact]
    public void ACycleBetweenRulesIsReportedAtBuildTime()
    {
        var draft = new DocumentDraft(RuleTestModels.Cycle());

        var diagnostic = Assert.Single(draft.Rules.Diagnostics);
        Assert.Equal(RuleDiagnosticCode.Cycle, diagnostic.Code);
        Assert.Contains("alpha", diagnostic.Message, StringComparison.Ordinal);
        Assert.Contains("beta", diagnostic.Message, StringComparison.Ordinal);

        // The form still works: the rules ran once in declaration order, nothing threw.
        Assert.True(draft.Fields["alpha"].IsVisible);
        Assert.True(draft.Fields["beta"].IsVisible);
    }

    [Fact]
    public void OnlyTheAffectedRulesReRun()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        var before = draft.Rules.EvaluationCount;

        // No rule reads "kind": changing it runs nothing at all.
        draft.Set("kind", new SelectFieldValue("Meal"));
        Assert.Equal(before, draft.Rules.EvaluationCount);

        // Four rules read "amount": justification's visibility and its mandatory rule,
        // total's read-only rule and discount_reason's visibility rule. Nothing else runs.
        draft.Set("amount", Amount("150.00"));
        Assert.Equal(before + 4, draft.Rules.EvaluationCount);

        // "status" has one rule of its own.
        before = draft.Rules.EvaluationCount;
        draft.Set("status", new SelectFieldValue("Paid"));
        Assert.Equal(before + 1, draft.Rules.EvaluationCount);
    }

    [Fact]
    public void ACountingContextSeesExactlyTheFieldReadsOfTheChangedRules()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        var reads = new RuleTestModels.Reads();
        draft.Rules.Observe = inner => new RuleTestModels.CountingContext(inner, reads);
        var before = draft.Rules.FieldReadCount;

        draft.Set("amount", Amount("20.00"));

        // The four rules that read doc.amount each read it once, and nothing else runs:
        // justification's visibility and its mandatory rule, total's read-only rule and
        // discount_reason's visibility rule.
        Assert.Equal(4, reads.Count);
        Assert.Equal(before + 4, draft.Rules.FieldReadCount);
    }

    [Fact]
    public void ChildRowRulesReadTheRowAndItsParent()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        var row = draft.Table("items").AddRow();

        // The row's note is shown by the parent document's has_receipt.
        Assert.False(row.Values["note"].IsVisible);
        draft.Set("has_receipt", new BoolFieldValue(true));
        Assert.True(row.Values["note"].IsVisible);

        // The row's line total is read-only while the parent has an amount and the row a
        // quantity: the rule reads both roots.
        Assert.False(row.Values["line_total"].IsReadOnly);
        draft.Set("amount", Amount("10.00"));
        Assert.False(row.Values["line_total"].IsReadOnly);
        row.SetValue("qty", Amount("2.00"));
        Assert.True(row.Values["line_total"].IsReadOnly);
    }

    [Fact]
    public void DecimalRulesAreExact()
    {
        var model = new DocTypeModel(
            "exact_rules",
            "Exact Rules",
            1,
            false,
            [
                new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
                new FieldModel("shown", "Shown", FieldKind.Text, dependsOn: "doc.amount > 0.10"),
            ],
            exprVersion: RuleTestModels.Version);
        var draft = new DocumentDraft(model);

        // 0.10 is not greater than 0.10 (scale is not value), and 0.11 is.
        draft.Set("amount", Amount("0.10"));
        Assert.False(draft.Fields["shown"].IsVisible);
        draft.Set("amount", Amount("0.11"));
        Assert.True(draft.Fields["shown"].IsVisible);

        // A value no double can hold exactly compares exactly.
        draft.Set("amount", new DecimalFieldValue(ExactDecimal.FromCanonical("12345678901234.56")));
        Assert.True(draft.Fields["shown"].IsVisible);
        Assert.Equal("12345678901234.56",
            Assert.IsType<DecimalFieldValue>(draft.Get("amount")).CanonicalText);
    }

    [Fact]
    public void AParseErrorDisablesOnlyItsOwnRule()
    {
        var model = new DocTypeModel(
            "broken",
            "Broken",
            1,
            false,
            [
                new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
                new FieldModel("broken", "Broken", FieldKind.Text, dependsOn: "doc.amount >"),
                new FieldModel("fine", "Fine", FieldKind.Text, dependsOn: "doc.amount > 0.00"),
            ],
            exprVersion: RuleTestModels.Version);
        var draft = new DocumentDraft(model);

        var diagnostic = Assert.Single(draft.Rules.Diagnostics);
        Assert.Equal(RuleDiagnosticCode.Parse, diagnostic.Code);
        Assert.Equal("broken", diagnostic.FieldPath);
        Assert.Equal(RuleKind.Visibility, diagnostic.Kind);
        Assert.NotNull(diagnostic.Offset);

        // The broken rule is ignored (the field stays as its static properties say); the
        // other field's rule still works.
        Assert.True(draft.Fields["broken"].IsVisible);
        Assert.False(draft.Fields["fine"].IsVisible);
        draft.Set("amount", Amount("1.00"));
        Assert.True(draft.Fields["fine"].IsVisible);
    }

    [Fact]
    public void ARuleThatIsNotABoolIsReportedAndIgnored()
    {
        var model = new DocTypeModel(
            "typed",
            "Typed",
            1,
            false,
            [
                new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
                new FieldModel("odd", "Odd", FieldKind.Text, dependsOn: "doc.amount"),
            ],
            exprVersion: RuleTestModels.Version);
        var draft = new DocumentDraft(model);

        Assert.Contains(draft.Rules.Diagnostics, diagnostic => diagnostic.Code == RuleDiagnosticCode.NotABool);
        Assert.True(draft.Fields["odd"].IsVisible);
    }

    [Fact]
    public void RowRulesRunWhenOnlyTheRowDocTypeCarriesExpressions()
    {
        var draft = new DocumentDraft(RuleTestModels.RowOnlyExpressions());
        Assert.True(draft.Rules.ExpressionsEnabled);

        var row = draft.Table("items").AddRow();
        Assert.False(row.Values["note"].IsVisible);

        // The document has no rule of its own, but the change still reaches the row's
        // rule through parent.flag.
        draft.Set("flag", new BoolFieldValue(true));
        Assert.True(row.Values["note"].IsVisible);
    }

    [Fact]
    public void AnUnsupportedExpressionVersionIgnoresEveryRule()
    {
        var model = new DocTypeModel(
            "future",
            "Future",
            1,
            false,
            [
                new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
                new FieldModel("later", "Later", FieldKind.Text, dependsOn: "doc.amount > 0.00",
                    mandatoryDependsOn: "doc.amount > 100.00"),
            ],
            exprVersion: RuleEngine.SupportedExpressionVersion + 1);
        var draft = new DocumentDraft(model);

        Assert.False(draft.Rules.ExpressionsEnabled);
        Assert.Contains(draft.Rules.Diagnostics,
            diagnostic => diagnostic.Code == RuleDiagnosticCode.UnsupportedExprVersion);

        // The static properties alone are in force.
        Assert.True(draft.Fields["later"].IsVisible);
        Assert.False(draft.Fields["later"].IsMandatory);
        draft.Set("amount", Amount("500.00"));
        Assert.False(draft.Fields["later"].IsMandatory);
        Assert.Equal(0, draft.Rules.EvaluationCount);
    }

    [Fact]
    public void RulesChangedIsRaisedOncePerChangeWithEveryAffectedField()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        var batches = new List<IReadOnlyList<string>>();
        draft.Rules.RulesChanged += (_, args) => batches.Add(args.Fields);

        draft.Set("amount", Amount("150.00"));

        var batch = Assert.Single(batches);
        // justification appears (shown and required), discount_reason appears; total's
        // read-only state did not move (it is statically read-only already).
        Assert.Contains("justification", batch);
        Assert.Contains("discount_reason", batch);
        Assert.DoesNotContain("total", batch);

        // A change no rule reacts to raises nothing.
        draft.Set("kind", new SelectFieldValue("Meal"));
        Assert.Single(batches);
    }

    [Fact]
    public void RulesReRunAfterALoadAndARevert()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        draft.LoadFrom("""{"amount":"150.00","has_receipt":true}""");

        Assert.True(draft.Fields["justification"].IsMandatory);
        Assert.True(draft.Fields["receipt_note"].IsVisible);
        Assert.False(draft.IsDirty);

        draft.Set("amount", Amount("1.00"));
        Assert.False(draft.Fields["justification"].IsMandatory);

        draft.Revert();
        Assert.True(draft.Fields["justification"].IsMandatory);
        Assert.True(draft.Fields["receipt_note"].IsVisible);
    }

    [Fact]
    public void ADocTypeWithoutExpressionsKeepsItsStaticState()
    {
        var draft = new DocumentDraft(TestModels.Invoice());

        Assert.False(draft.Rules.ExpressionsEnabled);
        Assert.Empty(draft.Rules.Diagnostics);
        Assert.True(draft.Fields["customer"].IsVisible);
        Assert.True(draft.Fields["customer"].IsMandatory);
        Assert.True(draft.Fields["grand_total"].IsReadOnly);
        Assert.False(draft.Fields["transaction_date"].IsReadOnly);
    }

    private static DecimalFieldValue Amount(string canonical) =>
        new(ExactDecimal.FromCanonical(canonical));
}
