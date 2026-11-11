using System.Globalization;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Metadata;
using Avalonia.Controls.Primitives;
using Avalonia.Data;
using Avalonia.Input;
using Avalonia.Interactivity;
using Calendar = Avalonia.Controls.Calendar;

namespace Spotledger.Client.Controls;

/// <summary>
/// A date entry: a text field with a configurable display pattern (default
/// <c>dd-MM-yyyy</c>) plus a calendar popup. The value is a <see cref="DateOnly"/>. Invalid
/// text keeps the previous value and shows <see cref="ErrorText"/>; empty text clears it.
/// </summary>
[TemplatePart("PART_TextBox", typeof(TextBox))]
[TemplatePart("PART_CalendarButton", typeof(Button))]
[TemplatePart("PART_Popup", typeof(Popup))]
[TemplatePart("PART_Calendar", typeof(Calendar))]
public class SpotDateInput : TemplatedControl
{
    public static readonly StyledProperty<DateOnly?> ValueProperty =
        AvaloniaProperty.Register<SpotDateInput, DateOnly?>(
            nameof(Value), defaultBindingMode: BindingMode.TwoWay);

    public static readonly StyledProperty<string> PatternProperty =
        AvaloniaProperty.Register<SpotDateInput, string>(nameof(Pattern), "dd-MM-yyyy");

    public static readonly StyledProperty<string?> PlaceholderProperty =
        AvaloniaProperty.Register<SpotDateInput, string?>(nameof(Placeholder));

    public static readonly StyledProperty<string?> ErrorTextProperty =
        AvaloniaProperty.Register<SpotDateInput, string?>(nameof(ErrorText));

    public static readonly StyledProperty<bool> IsCalendarOpenProperty =
        AvaloniaProperty.Register<SpotDateInput, bool>(nameof(IsCalendarOpen));

    private TextBox? _textBox;
    private Button? _calendarButton;
    private Popup? _popup;
    private Calendar? _calendar;
    private bool _suppressTextSync;

    /// <summary>The committed date; null when the field is empty.</summary>
    public DateOnly? Value
    {
        get => GetValue(ValueProperty);
        set => SetValue(ValueProperty, value);
    }

    /// <summary>The entry/display pattern, e.g. "dd-MM-yyyy".</summary>
    public string Pattern
    {
        get => GetValue(PatternProperty);
        set => SetValue(PatternProperty, value);
    }

    public string? Placeholder
    {
        get => GetValue(PlaceholderProperty);
        set => SetValue(PlaceholderProperty, value);
    }

    /// <summary>The validation message after rejected text; cleared on a successful commit.</summary>
    public string? ErrorText
    {
        get => GetValue(ErrorTextProperty);
        set => SetValue(ErrorTextProperty, value);
    }

    public bool IsCalendarOpen
    {
        get => GetValue(IsCalendarOpenProperty);
        set => SetValue(IsCalendarOpenProperty, value);
    }

    public TextBox? TextBoxControl => _textBox;

    public Popup? CalendarPopup => _popup;

    protected override void OnApplyTemplate(TemplateAppliedEventArgs e)
    {
        base.OnApplyTemplate(e);
        if (_calendarButton is not null)
        {
            _calendarButton.Click -= OnCalendarButtonClick;
        }

        if (_calendar is not null)
        {
            _calendar.PropertyChanged -= OnCalendarPropertyChanged;
        }

        _textBox = e.NameScope.Find<TextBox>("PART_TextBox");
        _calendarButton = e.NameScope.Find<Button>("PART_CalendarButton");
        _popup = e.NameScope.Find<Popup>("PART_Popup");
        _calendar = e.NameScope.Find<Calendar>("PART_Calendar");

        if (_textBox is not null)
        {
            _textBox.LostFocus += (_, _) => Commit();
            _textBox.KeyDown += OnTextBoxKeyDown;
        }

        if (_calendarButton is not null)
        {
            _calendarButton.Click += OnCalendarButtonClick;
        }

        if (_calendar is not null)
        {
            _calendar.PropertyChanged += OnCalendarPropertyChanged;
        }

        if (_popup is not null)
        {
            _popup.IsOpen = IsCalendarOpen;
            _popup.PropertyChanged += OnPopupPropertyChanged;
        }

        PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
        SyncTextToState();
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == ValueProperty && !_suppressTextSync)
        {
            SyncTextToState();
        }
        else if (change.Property == PatternProperty)
        {
            SyncTextToState();
        }
        else if (change.Property == ErrorTextProperty)
        {
            PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
        }
        else if (change.Property == IsCalendarOpenProperty && _popup is not null)
        {
            _popup.IsOpen = IsCalendarOpen;
        }
    }

    private void OnPopupPropertyChanged(object? sender, AvaloniaPropertyChangedEventArgs e)
    {
        if (e.Property == Popup.IsOpenProperty && sender is Popup popup && !popup.IsOpen)
        {
            IsCalendarOpen = false;
        }
    }

    private void OnTextBoxKeyDown(object? sender, KeyEventArgs e)
    {
        if (e.Key == Key.Enter)
        {
            Commit();
            e.Handled = true;
        }
        else if (e.Key == Key.Escape && IsCalendarOpen)
        {
            IsCalendarOpen = false;
            e.Handled = true;
        }
    }

    private void OnCalendarButtonClick(object? sender, RoutedEventArgs e)
    {
        IsCalendarOpen = !IsCalendarOpen;
    }

    private void OnCalendarPropertyChanged(object? sender, AvaloniaPropertyChangedEventArgs e)
    {
        if (e.Property != Calendar.SelectedDateProperty || sender is not Calendar calendar)
        {
            return;
        }

        if (calendar.SelectedDate is { } selected)
        {
            var dateOnly = DateOnly.FromDateTime(selected.Date);
            IsCalendarOpen = false;
            CommitValue(dateOnly);
        }
    }

    /// <summary>
    /// Parses the current text with <see cref="Pattern"/>. Valid text commits and clears the
    /// error; empty text clears the value; invalid text keeps the previous value and sets
    /// <see cref="ErrorText"/>.
    /// </summary>
    public bool Commit()
    {
        var text = (_textBox?.Text ?? string.Empty).Trim();
        if (text.Length == 0)
        {
            CommitValue(null);
            return true;
        }

        if (DateOnly.TryParseExact(text, Pattern, CultureInfo.InvariantCulture, DateTimeStyles.None, out var parsed))
        {
            CommitValue(parsed);
            return true;
        }

        ErrorText = $"Not a valid date — expected {Pattern}.";
        SyncTextToState();
        return false;
    }

    private void CommitValue(DateOnly? value)
    {
        _suppressTextSync = true;
        try
        {
            Value = value;
            ErrorText = null;
            SyncTextToState();
        }
        finally
        {
            _suppressTextSync = false;
        }
    }

    private void SyncTextToState()
    {
        if (_textBox is null || _textBox.IsFocused)
        {
            return;
        }

        _textBox.Text = Value is { } value
            ? value.ToString(Pattern, CultureInfo.InvariantCulture)
            : string.Empty;
        if (_calendar is not null)
        {
            _calendar.SelectedDate = Value is { } selected ? selected.ToDateTime(TimeOnly.MinValue) : null;
        }
    }
}
