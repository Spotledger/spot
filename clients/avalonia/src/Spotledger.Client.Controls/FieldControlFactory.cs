using System.Globalization;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;
using Avalonia.VisualTree;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>The only field-kind dispatch. Uses the existing decoded metadata surface, never wire guesses.</summary>
public static class FieldControlFactory
{
    private static readonly ConditionalWeakTable<Control, FieldControlBinding> Bindings = new();
    public static FieldControlBinding GetBinding(Control control) => Bindings.TryGetValue(control, out var binding) ? binding : throw new ArgumentException("Not created by this factory.", nameof(control));
    public static Control Create(FieldModel field, FieldState state)
    {
        ArgumentNullException.ThrowIfNull(state);
        if (state.IsMasked)
        {
            var masked = new SpotMaskedField { Name = field.Name };
            AutomationProperties.SetName(masked, field.Label);
            var maskedBinding = new FieldControlBinding(_ => { }, () => (null, null), true, _ => { }, masked.Load);
            Bindings.Add(masked, maskedBinding);
            maskedBinding.Load(state);
            return masked;
        }
        var control = Create(field);
        var binding = GetBinding(control);
        binding.Load(state);
        control.AddHandler(InputElement.LostFocusEvent, (_, _) => binding.CommitTo(state), RoutingStrategies.Bubble);
        return control;
    }
    /// <summary>Currency presentation uses the exact draft path, including child rows.
    /// Recreate on masking transitions; refresh other state with binding.Load(state, currency).</summary>
    public static Control Create(FieldModel field, FieldState state, DocumentDraft draft, string fieldPath)
    {
        var control = Create(field, state);
        GetBinding(control).Load(state, draft.Currency.GetValueOrDefault(fieldPath));
        return control;
    }
    public static Control Create(FieldModel field)
    {
        ArgumentNullException.ThrowIfNull(field);
        FieldValue loadedValue = FieldValue.Null;
        Control control;
        Action<FieldValue> load;
        Func<(FieldValue?, FieldError?)> read;
        (FieldValue?, FieldError?) Parse(string? text)
        {
            if (string.Equals(text, FormatValue(loadedValue), StringComparison.Ordinal)) return (loadedValue, null);
            if (field.Kind is FieldKind.Text or FieldKind.LongText) return (new TextFieldValue(text ?? string.Empty), null);
            return field.TryParse(text?.Trim(), out var value, out var error, NumberParsingSettings.WithSeparators) ? (value, null) : (null, error);
        }
        if (field.ReadOnly && field.Kind != FieldKind.Secret && field.Kind != FieldKind.Unknown && Enum.IsDefined(field.Kind)
            && !(field.Kind == FieldKind.DecimalNumber && field.Format == "currency"))
        {
            var display = new TextBox { IsReadOnly = true, TextAlignment = field.Kind is FieldKind.DecimalNumber or FieldKind.IntegerNumber ? TextAlignment.Right : TextAlignment.Start };
            control = display;
            load = value => display.Text = FormatValue(value);
            read = () => (null, null);
        }
        else switch (field.Kind)
        {
            case FieldKind.Text when field.Hints.Control == ControlHint.Textarea:
            {
                var input = new SpotTextArea();
                control = input;
                load = value => input.Text = FormatValue(value);
                read = () => Parse(input.Text);
                break;
            }
            case FieldKind.Text:
            case FieldKind.IntegerNumber:
            case FieldKind.Bytes:
            {
                var input = new SpotTextInput { MaxLength = field.MaxChars ?? 0 };
                control = input;
                load = value => input.Text = FormatValue(value);
                read = () => { var result = Parse(input.Text); input.ErrorText = result.Item2?.Message; return result; };
                break;
            }
            case FieldKind.LongText:
            case FieldKind.Json:
            {
                var input = new SpotTextArea { Format = field.Format };
                control = input;
                load = value => input.Text = FormatValue(value);
                read = () => Parse(input.Text);
                break;
            }
            case FieldKind.DecimalNumber when field.Format == "currency":
            {
                var input = new SpotCurrencyInput { IsReadOnly = field.ReadOnly };
                control = input;
                load = value => input.Text = FormatValue(value);
                read = () => { var result = Parse(input.Text); input.ErrorText = result.Item2?.Message; return result; };
                break;
            }
            case FieldKind.DecimalNumber when (field.Precision ?? 38) > 28 || field.Scale > 28:
            {
                // Metadata supports 38 digits; a native-decimal editor must never round a larger value.
                var input = new SpotTextInput();
                control = input;
                load = value => input.Text = FormatValue(value);
                read = () => { var result = Parse(input.Text); input.ErrorText = result.Item2?.Message; return result; };
                break;
            }
            case FieldKind.DecimalNumber:
            {
                SpotDecimalInput input = field.Format == "percent" ? new SpotPercentInput() : new SpotDecimalInput();
                input.Scale = field.Scale ?? 0;
                control = input;
                load = value =>
                {
                    if (value is DecimalFieldValue { IsNative: true } number) input.Value = number.ExactValue.AsDecimal;
                    else input.Value = null;
                };
                read = () => { var result = Parse(input.Text); input.ErrorText = result.Item2?.Message; return result; };
                break;
            }
            case FieldKind.Bool:
            {
                if (field.Hints.Control == ControlHint.Switch)
                {
                    var toggle = new SpotSwitch();
                    control = toggle;
                    load = value => toggle.IsChecked = (value as BoolFieldValue)?.Value;
                    read = () => (toggle.IsChecked is { } value ? new BoolFieldValue(value) : FieldValue.Null, null);
                    break;
                }
                var input = new SpotCheckBox();
                control = input;
                load = value => input.IsChecked = (value as BoolFieldValue)?.Value;
                read = () => (input.IsChecked is { } value ? new BoolFieldValue(value) : FieldValue.Null, null);
                break;
            }
            case FieldKind.Select:
            {
                if (field.Multiple)
                {
                    var multiple = new SpotSelectMultiple { Options = field.Options };
                    control = multiple;
                    load = value => multiple.SelectedValues = (value as MultiSelectFieldValue)?.Values ?? [];
                    read = () => (new MultiSelectFieldValue(field.Options.Where(x => multiple.SelectedValues.Contains(x, StringComparer.Ordinal)).ToArray()), null);
                    break;
                }
                var input = new SpotSelect { Options = field.Options.Select(x => new SelectOption(x, x)).ToArray() };
                control = input;
                load = value => input.SelectedItem = input.Options?.FirstOrDefault(x => Equals(x.Value, (value as SelectFieldValue)?.Value));
                read = () => Parse((input.SelectedItem as SelectOption)?.Value as string);
                break;
            }
            case FieldKind.Date:
            {
                var input = new SpotDateInput();
                control = input;
                load = value => input.Value = (value as DateFieldValue)?.Value;
                read = () => input.TextBoxControl is null || input.Commit() ? (input.Value is { } value ? new DateFieldValue(value) : FieldValue.Null, null) : Parse(input.TextBoxControl.Text);
                break;
            }
            case FieldKind.Time:
            {
                var input = new SpotTimeInput();
                control = input;
                load = value => input.Value = (value as TimeFieldValue)?.Value;
                read = () => input.Commit() ? (input.Value is { } value ? new TimeFieldValue(value) : FieldValue.Null, null) : Parse(input.Text);
                break;
            }
            case FieldKind.Datetime:
            {
                var input = new SpotDateTimeInput();
                control = input;
                load = value => input.Value = (value as DateTimeFieldValue)?.Value;
                read = () => input.Commit() ? (input.Value is { } value ? new DateTimeFieldValue(value) : FieldValue.Null, null) : Parse(input.Text);
                break;
            }
            case FieldKind.Link:
            case FieldKind.DynamicLink:
            {
                var input = new SpotLookupBox();
                control = input;
                load = value => { var link = value as LinkFieldValue; input.SelectedItem = link?.Id is { } id ? new LookupItem(id, link.DisplayText ?? id) : null; };
                read = () => (new LinkFieldValue(input.SelectedItem?.Id, input.SelectedItem?.Label), null);
                break;
            }
            case FieldKind.Secret:
            {
                var input = new SpotSecretInput { IsReadOnly = field.ReadOnly };
                control = input;
                load = value => input.Value = value as SecretFieldValue ?? new SecretFieldValue(false);
                read = () => (input.Value, null);
                break;
            }
            case FieldKind.Attach:
            {
                var input = new SpotAttachControl();
                control = input;
                load = value => input.FileId = (value as AttachFieldValue)?.FileId;
                read = () => (new AttachFieldValue(input.FileId), null);
                break;
            }
            case FieldKind.Table when !string.IsNullOrEmpty(field.LinkField):
            {
                var input = new SpotMultiSelect();
                var loaded = new TableFieldValue([], null);
                control = input;
                load = value =>
                {
                    loaded = value as TableFieldValue ?? new TableFieldValue([], null);
                    input.SelectedItems = loaded.Rows.Select(row => row.Values.GetValueOrDefault(field.LinkField) as LinkFieldValue).Where(x => x?.Id is not null).Select(x => new LookupItem(x!.Id!, x.DisplayText ?? x.Id!)).ToArray();
                };
                read = () => (new TableFieldValue(input.SelectedItems.Select(item => loaded.Rows.FirstOrDefault(row => (row.Values.GetValueOrDefault(field.LinkField) as LinkFieldValue)?.Id == item.Id) ?? TableRowValue.New(new Dictionary<string, FieldValue> { [field.LinkField] = new LinkFieldValue(item.Id, item.Label) })).ToArray(), loaded.RowModel), null);
                break;
            }
            case FieldKind.Table:
            {
                var input = new SpotTableControl();
                control = input;
                load = value => input.Value = value as TableFieldValue ?? new TableFieldValue([], null);
                read = () => (input.TryCommit() ? input.Value : null, null);
                break;
            }
            default:
            {
                var display = new TextBox { IsReadOnly = true };
                control = display;
                load = _ => display.Text = "[Unsupported field kind: " + (field.UnknownKindName ?? field.Kind.ToString()) + "]";
                read = () => (null, null);
                break;
            }
        }
        AutomationProperties.SetName(control, field.Label);
        AutomationProperties.SetHelpText(control, field.Hints.Description ?? string.Empty);
        control.Name = field.Name;
        var binding = new FieldControlBinding(value => { loadedValue = value; load(value); }, read, field.ReadOnly || !Enum.IsDefined(field.Kind) || field.Kind == FieldKind.Unknown, value => loadedValue = value,
            state => ApplyPresentation(control, state, field.ReadOnly));
        if (control is SpotCurrencyInput currencyInput) binding.CurrencyChanged = currency => currencyInput.CurrencyInfo = currency;
        Bindings.Add(control, binding);
        binding.Load(field.Default ?? field.Kind.EmptyValue());
        return control;
    }
    /// <summary>Presentation of values only; secret values can contain only the safe IsSet flag.</summary>
    internal static string FormatValue(FieldValue value) => value switch
    {
        TextFieldValue text => text.Value,
        IntFieldValue integer => integer.Value.ToString(CultureInfo.InvariantCulture),
        DecimalFieldValue number => number.CanonicalText,
        BoolFieldValue flag => flag.Value ? "True" : "False",
        DateFieldValue date => date.Value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture),
        TimeFieldValue time => time.Value.ToString("HH:mm:ss.FFFFFFF", CultureInfo.InvariantCulture),
        DateTimeFieldValue instant => instant.Value.ToString("O", CultureInfo.InvariantCulture),
        SelectFieldValue option => option.Value,
        MultiSelectFieldValue options => string.Join(", ", options.Values),
        LinkFieldValue link => link.DisplayText ?? link.Id ?? string.Empty,
        AttachFieldValue file => file.FileId ?? string.Empty,
        SecretFieldValue secret => secret.IsSet ? "Set" : "Not set",
        BytesFieldValue bytes => bytes.Base64,
        JsonFieldValue json => json.JsonText,
        TableFieldValue table => string.Join("\n", table.Rows.Select(row => string.Join(" | ", row.Values.Values.Select(FormatValue)))),
        _ => string.Empty,
    };
    private static void ApplyPresentation(Control control, FieldState state, bool readOnly)
    {
        if (state.IsMasked)
        {
            control.IsVisible = false;
            control.IsEnabled = false;
            if (control is TextBox oldText) oldText.IsUndoEnabled = false;
            foreach (var text in control.GetVisualDescendants().OfType<TextBox>()) text.IsUndoEnabled = false;
            throw new InvalidOperationException("Recreate the field control when masking changes.");
        }
        readOnly |= !state.IsEditable;
        if (control is CatalogueControl catalogue) catalogue.IsReadOnly = readOnly;
        else if (control is SpotTextInput input) input.IsReadOnly = readOnly;
        else if (control is TextBox text) text.IsReadOnly = readOnly;
        else control.IsEnabled = !readOnly;
        if (control is SpotAttachControl attachment)
        {
            attachment.MaximumBytes = state.AttachLimit?.MaxBytes;
            attachment.AllowedContentTypes = state.AttachLimit?.Accept;
        }
        if (control is SpotLookupBox lookup) lookup.CanCreateTarget = state.CanCreateTarget;
        if (control is SpotMultiSelect multiple) multiple.CanCreateTarget = state.CanCreateTarget;
    }
}
