using System.Windows.Input;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class FieldPresentationTests
{
    [AvaloniaFact]
    public void EveryKindUsesOnlyServerMaskAndAnAuthorizedRevealCommand()
    {
        foreach (var kind in Enum.GetValues<FieldKind>())
        {
            var field = new FieldModel("value", "Protected", kind);
            var draft = Draft(field, masked: true);
            var state = draft.Fields["value"];
            var input = Assert.IsType<SpotMaskedField>(FieldControlFactory.Create(field, state));
            var window = TestHost.Show(input);
            var command = new RecordingCommand();
            input.RevealCommand = command;
            Assert.Equal("server mask …42", input.DisplayText);
            Assert.All(input.GetVisualDescendants().OfType<TextBox>(), text => Assert.True(text.IsReadOnly));
            input.RequestReveal();
            Assert.Equal(1, command.Calls);
            Assert.True(FieldControlFactory.GetBinding(input).CommitTo(state));
            Assert.True(state.Value.IsNull);
            draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"revoked","data":{"doctypes":[{"doctype":"demo","fields":{"masked":["value"]}}]}}"""));
            FieldControlFactory.GetBinding(input).Load(state);
            input.RequestReveal();
            Assert.Equal(1, command.Calls);
            Assert.False(input.CanReveal);
            Assert.DoesNotContain(input.GetVisualDescendants().OfType<Button>(), button => button.IsVisible);
            window.Close();
        }
    }

    [AvaloniaFact]
    public void RevealedTextIsReadOnlyAndScrubbedOnHideDetachAndAncestorHide()
    {
        var field = new FieldModel("value", "Protected", FieldKind.Text);
        var draft = Draft(field, masked: true);
        var input = Assert.IsType<SpotMaskedField>(FieldControlFactory.Create(field, draft.Fields["value"]));
        var parent = new StackPanel();
        parent.Children.Add(input);
        var window = TestHost.Show(parent);
        void Reveal()
        {
            draft.ApplyRevealResponse("value", """{"data":{"value":"private-value"}}""");
            FieldControlFactory.GetBinding(input).Load(draft.Fields["value"]);
            Assert.Equal("private-value", input.DisplayText);
            Assert.True(input.IsRevealed);
        }
        Reveal();
        input.HideReveal();
        Assert.Equal("server mask …42", input.DisplayText);
        Reveal();
        input.IsVisible = false;
        input.IsVisible = true;
        Assert.Equal("server mask …42", input.DisplayText);
        Reveal();
        parent.IsVisible = false;
        parent.IsVisible = true;
        Assert.Equal("server mask …42", input.DisplayText);
        Reveal();
        parent.Children.Remove(input);
        parent.Children.Add(input);
        Assert.Equal("server mask …42", input.DisplayText);
        Assert.All(input.GetVisualDescendants().OfType<TextBox>(), text =>
        {
            Assert.NotEqual("private-value", text.Text);
            Assert.False(text.IsUndoEnabled);
        });
        Assert.True(draft.Fields["value"].Value.IsNull);
        window.Close();
    }

    [AvaloniaFact]
    public void MaskingTransitionScrubsTheOldEditorBeforeRequestingHostRecreation()
    {
        var field = new FieldModel("value", "Protected", FieldKind.Text);
        var draft = Draft(field);
        draft.LoadFrom("""{"value":"previous-private-value"}""");
        var input = Assert.IsType<SpotTextInput>(FieldControlFactory.Create(field, draft.Fields["value"]));
        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"mask","data":{"doctypes":[{"doctype":"demo","fields":{"masked":["value"]}}]}}"""));
        Assert.Throws<InvalidOperationException>(() => FieldControlFactory.GetBinding(input).Load(draft.Fields["value"]));
        Assert.Equal(string.Empty, input.Text);
        Assert.False(input.IsVisible);
        Assert.False(input.IsEnabled);
        Assert.IsType<SpotMaskedField>(FieldControlFactory.Create(field, draft.Fields["value"]));
    }

    [AvaloniaFact]
    public void CurrencyUsesExactDraftPathAndNeverRoundsOrWritesItsSymbol()
    {
        var field = new FieldModel("value", "Amount", FieldKind.DecimalNumber, format: "currency", precision: 38, scale: 2);
        var draft = Draft(field);
        draft.LoadFrom("""{"value":"123456789012345678901234567890123456.78","_currency":{"value":{"code":"PKR","symbol":"Rs"}}}""");
        var input = Assert.IsType<SpotCurrencyInput>(FieldControlFactory.Create(field, draft.Fields["value"], draft, "value"));
        Assert.Equal("Rs", input.CurrencyInfo!.Symbol);
        Assert.Equal("123456789012345678901234567890123456.78", input.Text);
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(draft.Fields["value"]));
        Assert.Equal(input.Text, Assert.IsType<DecimalFieldValue>(draft.Get("value")).CanonicalText);
        input.Text = "0.123";
        Assert.False(FieldControlFactory.GetBinding(input).CommitTo(draft.Fields["value"]));
        Assert.NotNull(input.ErrorText);
        draft.LoadSavedFrom("""{"value":"12.34"}""");
        FieldControlFactory.GetBinding(input).Load(draft.Fields["value"], draft.Currency.GetValueOrDefault("value"));
        Assert.Null(input.CurrencyInfo);
        Assert.Equal("12.34", input.Text);
    }

    [AvaloniaFact]
    public void ReadOnlyCurrencyRetainsServerSymbolAndSelectableExactText()
    {
        var field = new FieldModel("value", "Amount", FieldKind.DecimalNumber, format: "currency", precision: 38, scale: 2, readOnly: true);
        var draft = Draft(field);
        draft.LoadFrom("""{"value":"1.23","_currency":{"value":{"code":"PKR","symbol":"Rs"}}}""");
        var input = Assert.IsType<SpotCurrencyInput>(FieldControlFactory.Create(field, draft.Fields["value"], draft, "value"));
        var window = TestHost.Show(input);
        Assert.Equal("Rs", input.CurrencyInfo!.Symbol);
        Assert.True(input.Editor.IsReadOnly);
        Assert.Equal("1.23", input.Text);
        window.Close();
    }

    [AvaloniaFact]
    public void MultipleSelectKeepsDefinitionOrderAndTextValues()
    {
        var field = new FieldModel("value", "Choices", FieldKind.Select, options: ["first", "second", "third"], multiple: true);
        var draft = Draft(field);
        var input = Assert.IsType<SpotSelectMultiple>(FieldControlFactory.Create(field, draft.Fields["value"]));
        input.Toggle("third");
        input.Toggle("first");
        input.Toggle("unknown");
        Assert.Equal<string>(["first", "third"], input.SelectedValues);
        Assert.True(FieldControlFactory.GetBinding(input).CommitTo(draft.Fields["value"]));
        Assert.Equal<string>(["first", "third"], Assert.IsType<MultiSelectFieldValue>(draft.Get("value")).Values);
        input.Toggle("first");
        Assert.Equal<string>(["third"], input.SelectedValues);
        input.IsReadOnly = true;
        input.Toggle("third");
        Assert.Equal<string>(["third"], input.SelectedValues);
    }

    [AvaloniaFact]
    public void ClosedHintsUseCompatibleControlsAndOtherwiseSafelyFallBack()
    {
        foreach (var hint in Enum.GetValues<ControlHint>().Append((ControlHint)999))
        {
            var hints = new FieldHintsModel(Control: hint);
            var flag = FieldControlFactory.Create(new("flag", "Flag", FieldKind.Bool, hints: hints));
            Assert.Equal(hint == ControlHint.Switch ? typeof(SpotSwitch) : typeof(SpotCheckBox), flag.GetType());
            var text = FieldControlFactory.Create(new("text", "Text", FieldKind.Text, hints: hints));
            Assert.Equal(hint == ControlHint.Textarea ? typeof(SpotTextArea) : typeof(SpotTextInput), text.GetType());
            Assert.IsType<SpotSelect>(FieldControlFactory.Create(new("choice", "Choice", FieldKind.Select, options: ["a"], hints: hints)));
            Assert.IsType<SpotDateInput>(FieldControlFactory.Create(new("date", "Date", FieldKind.Date, hints: hints)));
        }
    }

    internal static DocumentDraft Draft(FieldModel field, bool masked = false)
    {
        var draft = new DocumentDraft(new DocTypeModel("demo", "Demo", 1, false, [field]));
        if (masked)
        {
            draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"masked","data":{"doctypes":[{"doctype":"demo","fields":{"masked":["value"],"revealable":["value"]}}]}}"""));
            draft.LoadFrom("""{"value":"server mask …42"}""");
        }
        return draft;
    }
}

internal sealed class RecordingCommand : ICommand
{
    public int Calls { get; private set; }
    public bool Allowed { get; set; } = true;
    public object? LastParameter { get; private set; }
    public bool CanExecute(object? parameter) => Allowed;
    public void Execute(object? parameter) { Calls++; LastParameter = parameter; }
    public event EventHandler? CanExecuteChanged { add { } remove { } }
}
