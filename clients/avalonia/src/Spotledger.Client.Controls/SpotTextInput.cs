using System.Diagnostics.CodeAnalysis;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Metadata;
using Avalonia.Controls.Primitives;
using Avalonia.Data;
using Avalonia.Interactivity;

namespace Spotledger.Client.Controls;

/// <summary>
/// A single-line text entry with placeholder, max length, read-only mode, a clear button
/// and an inline validation message. The danger border and message show when
/// <see cref="ErrorText"/> is set.
/// </summary>
[TemplatePart("PART_TextBox", typeof(TextBox))]
[TemplatePart("PART_ClearButton", typeof(Button))]
public class SpotTextInput : TemplatedControl
{
    public static readonly StyledProperty<string?> TextProperty =
        AvaloniaProperty.Register<SpotTextInput, string?>(nameof(Text), defaultBindingMode: BindingMode.TwoWay);

    public static readonly StyledProperty<string?> PlaceholderProperty =
        AvaloniaProperty.Register<SpotTextInput, string?>(nameof(Placeholder));

    public static readonly StyledProperty<int> MaxLengthProperty =
        AvaloniaProperty.Register<SpotTextInput, int>(nameof(MaxLength));

    public static readonly StyledProperty<bool> IsReadOnlyProperty =
        AvaloniaProperty.Register<SpotTextInput, bool>(nameof(IsReadOnly));

    public static readonly StyledProperty<string?> ErrorTextProperty =
        AvaloniaProperty.Register<SpotTextInput, string?>(nameof(ErrorText));

    public static readonly StyledProperty<bool> ShowClearButtonProperty =
        AvaloniaProperty.Register<SpotTextInput, bool>(nameof(ShowClearButton), true);

    private TextBox? _textBox;
    private Button? _clearButton;

    public string? Text
    {
        get => GetValue(TextProperty);
        set => SetValue(TextProperty, value);
    }

    public string? Placeholder
    {
        get => GetValue(PlaceholderProperty);
        set => SetValue(PlaceholderProperty, value);
    }

    public int MaxLength
    {
        get => GetValue(MaxLengthProperty);
        set => SetValue(MaxLengthProperty, value);
    }

    public bool IsReadOnly
    {
        get => GetValue(IsReadOnlyProperty);
        set => SetValue(IsReadOnlyProperty, value);
    }

    /// <summary>The validation message; non-null shows the danger border and the text.</summary>
    public string? ErrorText
    {
        get => GetValue(ErrorTextProperty);
        set => SetValue(ErrorTextProperty, value);
    }

    public bool ShowClearButton
    {
        get => GetValue(ShowClearButtonProperty);
        set => SetValue(ShowClearButtonProperty, value);
    }

    /// <summary>Command executed when the clear button is clicked (in addition to clearing).</summary>
    public static readonly RoutedEvent<ClearEventArgs> ClearedEvent =
        RoutedEvent.Register<SpotTextInput, ClearEventArgs>(nameof(Cleared), RoutingStrategies.Bubble);

    /// <summary>Raised when the user clears the field via the clear button.</summary>
    public event EventHandler<ClearEventArgs>? Cleared
    {
        add => AddHandler(ClearedEvent, value);
        remove => RemoveHandler(ClearedEvent, value);
    }

    /// <summary>The inner text editor — tests type into this.</summary>
    public TextBox? TextBoxControl => _textBox;

    protected override void OnApplyTemplate(TemplateAppliedEventArgs e)
    {
        base.OnApplyTemplate(e);
        if (_clearButton is not null)
        {
            _clearButton.Click -= OnClearButtonClick;
        }

        _textBox = e.NameScope.Find<TextBox>("PART_TextBox");
        _clearButton = e.NameScope.Find<Button>("PART_ClearButton");
        if (_clearButton is not null)
        {
            _clearButton.Click += OnClearButtonClick;
        }

        if (_textBox is not null)
        {
            // Keep the control's Text in step with what the user types in the editor.
            _textBox.PropertyChanged += OnTextBoxPropertyChanged;
        }

        UpdateErrorState();
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == ErrorTextProperty)
        {
            UpdateErrorState();
        }
    }

    private void OnTextBoxPropertyChanged(object? sender, AvaloniaPropertyChangedEventArgs e)
    {
        if (e.Property == TextBox.TextProperty && sender is TextBox textBox && !string.Equals(Text, textBox.Text, StringComparison.Ordinal))
        {
            Text = textBox.Text;
        }
    }

    private void UpdateErrorState()
    {
        PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
    }

    private void OnClearButtonClick(object? sender, RoutedEventArgs e)
    {
        if (IsReadOnly)
        {
            return;
        }

        Text = string.Empty;
        RaiseEvent(new ClearEventArgs(ClearedEvent));
        _textBox?.Focus();
    }

    /// <summary>Event args for <see cref="ClearedEvent"/>.</summary>
    public sealed class ClearEventArgs : RoutedEventArgs
    {
        public ClearEventArgs(RoutedEvent routedEvent)
            : base(routedEvent)
        {
        }
    }
}
