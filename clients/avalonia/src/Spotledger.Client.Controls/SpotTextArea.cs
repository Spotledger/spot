using System.Text.RegularExpressions;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Data;
using Avalonia.Media;
using Avalonia.Markup.Xaml.MarkupExtensions;

namespace Spotledger.Client.Controls;

/// <summary>Auto-growing long text editor. Markdown preview renders text, never HTML or script.</summary>
public sealed partial class SpotTextArea : CatalogueControl
{
    public static readonly StyledProperty<string?> TextProperty = AvaloniaProperty.Register<SpotTextArea, string?>(nameof(Text), defaultBindingMode: BindingMode.TwoWay);
    public static readonly StyledProperty<string?> FormatProperty = AvaloniaProperty.Register<SpotTextArea, string?>(nameof(Format));
    public static readonly StyledProperty<bool> IsPreviewOpenProperty = AvaloniaProperty.Register<SpotTextArea, bool>(nameof(IsPreviewOpen), coerce: (o, v) => v && ((SpotTextArea)o).Format == "markdown");
    public string? Text { get => GetValue(TextProperty); set => SetValue(TextProperty, value); }
    public string? Format { get => GetValue(FormatProperty); set => SetValue(FormatProperty, value); }
    public bool IsPreviewOpen { get => GetValue(IsPreviewOpenProperty); set => SetValue(IsPreviewOpenProperty, value); }
    public bool IsMonospace => Format is "code" or "monospace";
    public TextBox Editor { get; } = new() { AcceptsReturn = true, TextWrapping = TextWrapping.Wrap };
    private readonly TextBox _preview = Display("Markdown preview");
    private readonly Button _toggle;
    /// <summary>The maximum editor height; callers can bind a token-derived layout budget.</summary>
    public double MaximumEditorHeight { get => _scroll.MaxHeight; set => _scroll.MaxHeight = value; }
    private readonly ScrollViewer _scroll;
    public string PreviewText => MarkdownText().Replace(Text ?? string.Empty, "$1$2");
    [GeneratedRegex(@"(?m)^#{1,6}\s+|\*\*(.*?)\*\*|\[(.*?)\]\([^)]*\)")]
    private static partial Regex MarkdownText();
    public SpotTextArea() : base("Long text")
    {
        _scroll = new ScrollViewer { Content = Editor };
        _scroll.Bind(MaxHeightProperty, new DynamicResourceExtension("Size.TextArea.MaximumHeight"));
        _toggle = Action("Preview markdown", () => IsPreviewOpen = !IsPreviewOpen);
        AutomationProperties.SetName(Editor, "Long text editor");
        Body.Children.Add(_scroll);
        Body.Children.Add(_toggle);
        Body.Children.Add(_preview);
        Editor.PropertyChanged += (_, e) => { if (e.Property == TextBox.TextProperty) SetCurrentValue(TextProperty, Editor.Text); };
        UpdatePresentation();
    }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == FormatProperty) CoerceValue(IsPreviewOpenProperty);
        if (change.Property == TextProperty || change.Property == FormatProperty || change.Property == IsPreviewOpenProperty) UpdatePresentation();
    }
    protected override void UpdatePresentation()
    {
        if (_scroll is null) return;
        Editor.Text = Text;
        Editor.IsReadOnly = IsReadOnly;
        Editor.Bind(TextBox.FontFamilyProperty, new DynamicResourceExtension(IsMonospace ? "Font.Family.Code" : "Font.Family.Latin"));
        _preview.Text = PreviewText;
        _preview.AcceptsReturn = true;
        _preview.TextWrapping = TextWrapping.Wrap;
        _preview.IsVisible = IsPreviewOpen;
        _scroll.IsVisible = !IsPreviewOpen;
        _toggle.IsVisible = Format == "markdown";
    }
}
