using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

public sealed class FormLayoutTests
{
    [Fact]
    public void FallbackLayoutPreservesDefinitionOrderAndOmitsHiddenFields()
    {
        var model = new DocTypeModel("sample", "Sample", 1, false,
        [
            new FieldModel("z", "Last alphabetically", FieldKind.Text),
            new FieldModel("hidden", "Hidden", FieldKind.Text, hints: new FieldHintsModel(Hidden: true)),
            new FieldModel("a", "First alphabetically", FieldKind.Text),
        ]);
        var draft = new DocumentDraft(model);
        var layout = FormLayout.Build(model, draft);

        var tab = Assert.Single(layout.Tabs);
        var section = Assert.Single(tab.Sections);
        Assert.Equal("Sample", tab.Label);
        Assert.Collection(section.Fields,
            field => Assert.Equal("z", field.Name),
            field => Assert.Equal("a", field.Name));
        Assert.Empty(layout.Errors);
        Assert.False(draft.IsDirty);
    }

    [Theory]
    [InlineData(null, true)]
    [InlineData("", true)]
    [InlineData("provided", false)]
    public void FallbackHiddenMandatoryFieldsReportMissingValues(string? value, bool expectError)
    {
        var model = new DocTypeModel("sample", "Sample", 1, false,
        [new FieldModel("hidden", "Hidden", FieldKind.Text,
            hints: new FieldHintsModel(Hidden: true), required: true,
            defaultValue: value is null ? null : new TextFieldValue(value))]);
        var draft = new DocumentDraft(model);
        var layout = FormLayout.Build(model, draft);

        Assert.Empty(Assert.Single(Assert.Single(layout.Tabs).Sections).Fields);
        Assert.Equal(expectError, layout.Errors.Count != 0);
        if (expectError)
        {
            Assert.Equal("hidden", Assert.Single(layout.Errors).Field);
        }
        draft.Set("hidden", new TextFieldValue("filled"));
        Assert.Empty(FormLayout.Build(model, draft).Errors);
    }

    [Fact]
    public void DraftStatesExposeStaticMetadata()
    {
        var model = new DocTypeModel("sample", "Sample", 1, false,
        [
            new FieldModel("visible", "Visible", FieldKind.Text, required: true, readOnly: true),
            new FieldModel("hidden", "Hidden", FieldKind.Text, hints: new FieldHintsModel(Hidden: true)),
            new FieldModel("items", "Items", FieldKind.Table, readOnly: true),
        ]);
        var draft = new DocumentDraft(model);

        Assert.True(draft.Fields["visible"].IsVisible);
        Assert.True(draft.Fields["visible"].IsMandatory);
        Assert.True(draft.Fields["visible"].IsReadOnly);
        Assert.False(draft.Fields["hidden"].IsVisible);
        Assert.False(draft.Fields["hidden"].IsMandatory);
        Assert.False(draft.Fields["hidden"].IsReadOnly);
        Assert.True(draft.Fields["items"].IsReadOnly);
        draft.LoadFrom("{\"visible\":\"loaded\"}");
        draft.Set("visible", new TextFieldValue("edited"));
        draft.Revert();
        Assert.True(draft.Fields["visible"].IsMandatory);
        Assert.True(draft.Fields["visible"].IsReadOnly);
    }
}
