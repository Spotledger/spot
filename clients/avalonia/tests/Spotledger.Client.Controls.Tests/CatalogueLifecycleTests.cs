using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class CatalogueLifecycleTests
{
    [AvaloniaFact]
    public void TextBindingPreservesWhitespaceAndEmptyText()
    {
        var state = new FieldState(new TextFieldValue(" original "));
        var input = Assert.IsType<SpotTextInput>(FieldControlFactory.Create(new("note", "Note", FieldKind.Text), state));
        input.Text = "  typed  ";
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
        Assert.Equal("  typed  ", Assert.IsType<TextFieldValue>(state.Value).Value);
        input.Text = string.Empty;
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
        Assert.Equal(string.Empty, Assert.IsType<TextFieldValue>(state.Value).Value);
    }

    [AvaloniaFact]
    public void UntouchedNullTextIsNotTurnedIntoAnEmptyString()
    {
        var state = new FieldState(FieldValue.Null);
        var input = Assert.IsType<SpotTextInput>(FieldControlFactory.Create(new("note", "Note", FieldKind.Text), state));
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
        Assert.Same(FieldValue.Null, state.Value);
        Assert.False(state.IsDirty);
        input.Text = "typed";
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
        input.Text = string.Empty;
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
        Assert.Equal(string.Empty, Assert.IsType<TextFieldValue>(state.Value).Value);
    }

    [AvaloniaFact]
    public void WideDecimalsUseMetadataExactTextInsteadOfNativeRounding()
    {
        var field = new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 9, precision: 38);
        Assert.True(field.TryParse("12345678901234567890123456789.123456789", out var value, out _));
        var state = new FieldState(value!);
        var input = Assert.IsType<SpotTextInput>(FieldControlFactory.Create(field, state));
        Assert.Equal("12345678901234567890123456789.123456789", input.Text);
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
        Assert.Equal(input.Text, Assert.IsType<DecimalFieldValue>(state.Value).CanonicalText);
    }

    [AvaloniaFact]
    public void UnspecifiedDecimalPrecisionStillPreservesWideMetadataValues()
    {
        var field = new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 9);
        Assert.True(field.TryParse("12345678901234567890123456789.123456789", out var value, out _));
        var state = new FieldState(value!);
        var input = Assert.IsType<SpotTextInput>(FieldControlFactory.Create(field, state));
        Assert.Equal("12345678901234567890123456789.123456789", input.Text);
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
    }

    [AvaloniaFact]
    public async Task EscapeCancelsInFlightSearchAndRestoresTheSavedLabel()
    {
        var source = new PendingLookupSource();
        var input = new SpotLookupBox { Source = source, Delay = new ImmediateLookupDelay(), SelectedItem = new("saved", "Saved label") };
        var window = TestHost.Show(input);
        input.Editor.Text = "replacement";
        var request = source.Calls.Single();
        input.Editor.Focus();
        window.KeyPress(Key.Escape, RawInputModifiers.None, PhysicalKey.Escape, null);
        Assert.True(request.Token.IsCancellationRequested);
        Assert.Equal("Saved label", input.Editor.Text);
        request.Result.SetResult([new("late", "Late")]);
        await input.PendingSearch;
        Assert.False(input.IsOpen);
        Assert.Equal("saved", input.SelectedItem!.Id);
        window.Close();
    }

    [AvaloniaFact]
    public void UnknownReadOnlyKindStillHasAClearlyMarkedPlaceholder()
    {
        var control = Assert.IsType<TextBox>(FieldControlFactory.Create(new("future", "Future", FieldKind.Unknown, unknownKindName: "new_kind", readOnly: true)));
        Assert.Contains("Unsupported", control.Text);
        Assert.Contains("new_kind", control.Text);
    }
}
