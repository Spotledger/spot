using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 5: the resolved layout. A field hidden by a **rule** is omitted and is not
/// a layout error (the rule may show it again); a mandatory field with no value that the
/// **layout** hides is reported (ADR-068 §7.5), never silently dropped. A view, when the
/// host has one, decides the order; otherwise the DocType's field order does.
/// </summary>
public sealed class RuleLayoutTests
{
    private static DocTypeModel Model() => new(
        "layout_sample",
        "Layout Sample",
        version: 1,
        isPosted: false,
        [
            new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("justification", "Justification", FieldKind.Text,
                dependsOn: "doc.amount > 0.00", mandatoryDependsOn: "doc.amount > 100.00"),
            new FieldModel("note", "Note", FieldKind.Text, required: true),
        ],
        exprVersion: RuleTestModels.Version);

    [Fact]
    public void AFieldHiddenByARuleIsOmittedAndIsNotALayoutError()
    {
        var model = Model();
        var draft = new DocumentDraft(model);

        // justification is hidden by its rule and (while hidden) has no value.
        var layout = FormLayout.Build(model, draft);
        var fields = Assert.Single(Assert.Single(layout.Tabs).Sections).Fields;

        Assert.DoesNotContain(fields, field => field.Name == "justification");
        Assert.Empty(layout.Errors);

        // Once the rule shows it and makes it mandatory, it is in the layout.
        draft.Set("amount", new DecimalFieldValue(ExactDecimal.FromCanonical("150.00")));
        layout = FormLayout.Build(model, draft);
        Assert.Contains(Assert.Single(Assert.Single(layout.Tabs).Sections).Fields,
            field => field.Name == "justification");
        Assert.True(draft.Fields["justification"].IsMandatory);
    }

    [Fact]
    public void AMandatoryFieldTheViewHidesIsALayoutError()
    {
        var model = Model();
        var draft = new DocumentDraft(model);
        var view = new FormView([
            new FormViewTab("Main", [
                new FormViewSection("Basics", ["amount"]),
            ]),
        ]);

        var layout = FormLayout.Build(model, draft, view);

        // note is statically required, has no value and the view does not place it.
        var error = Assert.Single(layout.Errors);
        Assert.Equal("note", error.Field);
        Assert.Equal("layout.hidden_required", error.Code);
    }

    [Fact]
    public void AViewDecidesTheOrderAndReportsAFieldItCannotPlace()
    {
        var model = Model();
        var draft = new DocumentDraft(model);
        var view = new FormView([
            new FormViewTab("Main", [
                new FormViewSection("First", ["note"]),
                new FormViewSection("Second", ["amount", "missing"]),
            ]),
        ]);

        var layout = FormLayout.Build(model, draft, view);

        var tab = Assert.Single(layout.Tabs);
        Assert.Equal("Main", tab.Label);
        Assert.Collection(tab.Sections,
            section =>
            {
                Assert.Equal("First", section.Label);
                Assert.Equal("note", Assert.Single(section.Fields).Name);
            },
            section =>
            {
                Assert.Equal("Second", section.Label);
                Assert.Equal("amount", Assert.Single(section.Fields).Name);
            });
        var error = Assert.Single(layout.Errors);
        Assert.Equal("missing", error.Field);
        Assert.Equal("layout.unknown_field", error.Code);
    }

    [Fact]
    public void AFieldHiddenByItsRuleIsOmittedFromAViewToo()
    {
        var model = Model();
        var draft = new DocumentDraft(model);
        var view = new FormView([
            new FormViewTab("Main", [
                new FormViewSection("Basics", ["amount", "justification", "note"]),
            ]),
        ]);

        var layout = FormLayout.Build(model, draft, view);
        Assert.Empty(layout.Errors);
        var placed = Assert.Single(Assert.Single(layout.Tabs).Sections).Fields;
        Assert.Equal(["amount", "note"], placed.Select(field => field.Name).ToArray());
    }

    [Fact]
    public void LayoutBuildingNeverChangesTheDraft()
    {
        var model = Model();
        var draft = new DocumentDraft(model);
        var before = draft.ToPatch();

        _ = FormLayout.Build(model, draft, new FormView([
            new FormViewTab("Main", [new FormViewSection("Basics", ["amount", "note"])]),
        ]));

        Assert.Equal(before, draft.ToPatch());
        Assert.False(draft.IsDirty);
    }
}
