using System.Globalization;
using System.Text.RegularExpressions;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Data;
using Avalonia.Input;

namespace Spotledger.Client.Controls;

/// <summary>A percentage in percentage points, never a floating-point fraction.</summary>
public sealed class SpotPercentInput : SpotDecimalInput
{
    protected override Type StyleKeyOverride => typeof(SpotDecimalInput);
    public SpotPercentInput() => AutomationProperties.SetName(this, "Percent");
}

/// <summary>Shared commit behaviour for selectable temporal editors. Invalid edits keep the value.</summary>
public abstract class SpotTemporalInput : TextBox
{
    public static readonly StyledProperty<string?> ErrorTextProperty =
        AvaloniaProperty.Register<SpotTemporalInput, string?>(nameof(ErrorText));
    public string? ErrorText { get => GetValue(ErrorTextProperty); set => SetValue(ErrorTextProperty, value); }
    protected override Type StyleKeyOverride => typeof(TextBox);
    protected abstract bool ParseValue(string text);
    protected abstract string FormatValue();
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property.Name == "Value") Text = FormatValue();
        if (change.Property == ErrorTextProperty) PseudoClasses.Set(":error", ErrorText is not null);
    }
    protected override void OnLostFocus(FocusChangedEventArgs e) { base.OnLostFocus(e); Commit(); }
    protected override void OnKeyDown(KeyEventArgs e)
    {
        if (e.Key == Key.Enter) { Commit(); e.Handled = true; }
        base.OnKeyDown(e);
    }
    public bool Commit()
    {
        if (IsReadOnly) return false;
        var raw = Text?.Trim() ?? string.Empty;
        if (!ParseValue(raw)) { ErrorText = "Enter a valid time with an explicit offset for an instant."; return false; }
        Text = FormatValue();
        ErrorText = null;
        return true;
    }
}

/// <summary>A wall-clock time, with no implicit date or time-zone conversion.</summary>
public sealed class SpotTimeInput : SpotTemporalInput
{
    public SpotTimeInput() => AutomationProperties.SetName(this, "Time");
    public static readonly StyledProperty<TimeOnly?> ValueProperty = AvaloniaProperty.Register<SpotTimeInput, TimeOnly?>(nameof(Value), defaultBindingMode: BindingMode.TwoWay);
    public TimeOnly? Value { get => GetValue(ValueProperty); set => SetValue(ValueProperty, value); }
    protected override bool ParseValue(string text)
    {
        if (text.Length == 0) { Value = null; return true; }
        if (!TimeOnly.TryParse(text, CultureInfo.InvariantCulture, DateTimeStyles.None, out var value)) return false;
        SetCurrentValue(ValueProperty, value);
        return true;
    }
    protected override string FormatValue() => Value?.ToString("HH:mm:ss.FFFFFFF", CultureInfo.InvariantCulture) ?? string.Empty;
}

/// <summary>An instant displayed in its own offset. Offset-less pastes are rejected, never localized.</summary>
public sealed partial class SpotDateTimeInput : SpotTemporalInput
{
    public SpotDateTimeInput() => AutomationProperties.SetName(this, "Date and time");
    [GeneratedRegex(@"(?:[zZ]|[+-]\d{2}:\d{2})$")]
    private static partial Regex ExplicitOffset();
    public static readonly StyledProperty<DateTimeOffset?> ValueProperty = AvaloniaProperty.Register<SpotDateTimeInput, DateTimeOffset?>(nameof(Value), defaultBindingMode: BindingMode.TwoWay);
    public DateTimeOffset? Value { get => GetValue(ValueProperty); set => SetValue(ValueProperty, value); }
    protected override bool ParseValue(string text)
    {
        if (text.Length == 0) { Value = null; return true; }
        if (!ExplicitOffset().IsMatch(text) || !DateTimeOffset.TryParse(text, CultureInfo.InvariantCulture, DateTimeStyles.None, out var value)) return false;
        SetCurrentValue(ValueProperty, value);
        return true;
    }
    protected override string FormatValue() => Value?.ToString("yyyy-MM-dd'T'HH:mm:ss.FFFFFFFzzz", CultureInfo.InvariantCulture) ?? string.Empty;
}
