using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class FieldFactoryTests
{
    [AvaloniaFact]
    public void FactoryCoversEveryExistingKindAndUnknownDoesNotThrow()
    {
        foreach (var kind in Enum.GetValues<FieldKind>())
        {
            var field = new FieldModel("field", "Field label", kind);
            var control = FieldControlFactory.Create(field);
            Assert.NotNull(control);
            Assert.Equal(field.Label, AutomationProperties.GetName(control));
            Assert.NotNull(FieldControlFactory.GetBinding(control));
            var readOnly = FieldControlFactory.Create(new FieldModel("field", "Field", kind, readOnly: true));
            if (kind != FieldKind.Secret) Assert.IsAssignableFrom<TextBox>(readOnly);
        }
        var future = FieldControlFactory.Create(new FieldModel("future", "Future", (FieldKind)999, unknownKindName: "future_kind"));
        Assert.Contains("Unsupported", Assert.IsAssignableFrom<TextBox>(future).Text);
    }

    [AvaloniaFact]
    public void FormatsAndLinkFieldSelectTheSpecializedControls()
    {
        Assert.IsType<SpotPercentInput>(FieldControlFactory.Create(new("p", "Percent", FieldKind.DecimalNumber, format: "percent", precision: 28)));
        Assert.IsType<SpotCurrencyInput>(FieldControlFactory.Create(new("c", "Amount", FieldKind.DecimalNumber, format: "currency", precision: 28)));
        Assert.IsType<SpotTextArea>(FieldControlFactory.Create(new("t", "Note", FieldKind.LongText, format: "markdown")));
        Assert.IsType<SpotMultiSelect>(FieldControlFactory.Create(new("m", "Tags", FieldKind.Table, linkField: "reference")));
        Assert.IsType<SpotTableControl>(FieldControlFactory.Create(new("t", "Rows", FieldKind.Table)));
    }

    [AvaloniaFact]
    public void BindingLoadsAndCommitsThroughTheMetadataParserWithoutRounding()
    {
        var field = new FieldModel("quantity", "Quantity", FieldKind.DecimalNumber, scale: 3, precision: 12);
        var state = new FieldState(new DecimalFieldValue(12.125m));
        var control = Assert.IsType<SpotDecimalInput>(FieldControlFactory.Create(field, state));
        Assert.Equal(12.125m, control.Value);
        control.Text = "99.1234";
        Assert.False(FieldControlFactory.GetBinding(control).CommitTo(state));
        Assert.Equal("12.125", Assert.IsType<DecimalFieldValue>(state.Value).CanonicalText);
        Assert.Single(state.Errors);
        control.Text = "99.123";
        Assert.True(FieldControlFactory.GetBinding(control).CommitTo(state));
        Assert.Equal("99.123", Assert.IsType<DecimalFieldValue>(state.Value).CanonicalText);
        Assert.Empty(state.Errors);
    }

    [AvaloniaFact]
    public void MultiSelectBindingPreservesExistingRowIdentityAndValues()
    {
        var field = new FieldModel("links", "Links", FieldKind.Table, linkField: "reference");
        var original = TableRowValue.Existing("row-id", new Dictionary<string, FieldValue> { ["reference"] = new LinkFieldValue("one", "One"), ["other"] = new TextFieldValue("Keep") });
        var state = new FieldState(new TableFieldValue([original], null));
        var input = Assert.IsType<SpotMultiSelect>(FieldControlFactory.Create(field, state));
        input.Add(new("two", "Two"));
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
        var rows = Assert.IsType<TableFieldValue>(state.Value).Rows;
        Assert.Same(original, rows[0]);
        Assert.Null(rows[1].Id);
        Assert.Equal("two", Assert.IsType<LinkFieldValue>(rows[1].Values["reference"]).Id);
    }
}
