using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Documents;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.Controls;
using Spotledger.Client.Extensions;
using TextBlock = Avalonia.Controls.TextBlock;

namespace Spotledger.Client.Extensions.Renderer;

/// <summary>
/// Draws a <c>text</c> block's restricted markdown: paragraphs, emphasis, strong, inline
/// code and lists. A link is a button that asks the host to open a registered destination;
/// nothing here fetches a URL, renders markup or opens an external page. The host passes
/// the open callback, so the shell keeps the confirmation decision.
/// </summary>
public sealed class MarkdownPresenter : UserControl
{
    private readonly StackPanel _body = new();
    private MarkdownModel? _model;
    private Func<MarkdownLink, CancellationToken, Task>? _open;

    /// <summary>Creates the presenter.</summary>
    public MarkdownPresenter()
    {
        Content = _body;
        _body.Orientation = Orientation.Vertical;
        Loaded += (_, _) => ApplyTheme();
        ActualThemeVariantChanged += (_, _) => ApplyTheme();
    }

    /// <inheritdoc/>
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == Density.ModeProperty)
        {
            ApplyTheme();
        }
    }

    /// <summary>Sets the parsed markdown and the callback a destination link invokes.</summary>
    public void Show(MarkdownModel model, Func<MarkdownLink, CancellationToken, Task>? open)
    {
        ArgumentNullException.ThrowIfNull(model);
        _model = model;
        _open = open;
        Build();
    }

    /// <summary>Re-applies the theme's typography after a theme or density change.</summary>
    public void ApplyTheme() => Build();

    private void Build()
    {
        _body.Children.Clear();
        _body.Spacing = ThemeAccess.FieldGap(this);
        if (_model is null)
        {
            return;
        }
        foreach (MarkdownBlock block in _model.Blocks)
        {
            switch (block)
            {
                case MarkdownParagraph paragraph:
                    _body.Children.Add(InlineHost(paragraph.Inlines, Orientation.Horizontal));
                    break;
                case MarkdownList list:
                    {
                        StackPanel items = new() { Orientation = Orientation.Vertical, Spacing = ThemeAccess.FieldGap(this) / 2 };
                        for (int index = 0; index < list.Items.Count; index++)
                        {
                            StackPanel line = new() { Orientation = Orientation.Horizontal, Spacing = ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenSpace1, 4) };
                            TextBlock marker = new() { Text = MarkdownModel.ItemMarker(list.Ordered, index) };
                            ThemeAccess.Body(marker, this);
                            line.Children.Add(marker);
                            line.Children.Add(InlineHost(list.Items[index], Orientation.Horizontal));
                            items.Children.Add(line);
                        }
                        _body.Children.Add(items);
                        break;
                    }
                default:
                    break;
            }
        }
    }

    private WrapPanel InlineHost(IReadOnlyList<MarkdownInline> inlines, Orientation orientation)
    {
        WrapPanel panel = new() { Orientation = orientation };
        foreach (MarkdownInline inline in inlines)
        {
            switch (inline)
            {
                case MarkdownText text:
                    panel.Children.Add(Text(text.Text, FontWeight.Normal, italic: false, code: false));
                    break;
                case MarkdownEmphasis emphasis:
                    panel.Children.Add(Text(Plain(emphasis.Children), FontWeight.Normal, italic: true, code: false));
                    break;
                case MarkdownStrong strong:
                    panel.Children.Add(Text(Plain(strong.Children), ThemeAccess.Weight(this, Spotledger.Client.Theme.Tokens.TokenFontWeightBold, FontWeight.Bold), italic: false, code: false));
                    break;
                case MarkdownCode code:
                    panel.Children.Add(Text(code.Text, FontWeight.Normal, italic: false, code: true));
                    break;
                case MarkdownLink link:
                    {
                        SpotButton button = new() { Variant = SpotButtonVariant.Subtle, Content = link.Text };
                        AutomationProperties.SetName(button, link.Text);
                        button.Click += async (_, _) =>
                        {
                            if (_open is not null)
                            {
                                await _open(link, CancellationToken.None).ConfigureAwait(true);
                            }
                        };
                        panel.Children.Add(button);
                        break;
                    }
                default:
                    break;
            }
        }
        return panel;
    }

    private TextBlock Text(string content, FontWeight weight, bool italic, bool code)
    {
        TextBlock text = new() { Text = content, FontWeight = weight, FontStyle = italic ? FontStyle.Italic : FontStyle.Normal };
        ThemeAccess.Body(text, this);
        ThemeAccess.ApplyScriptFont(text, this, content);
        if (code && ThemeAccess.Brush(this, Spotledger.Client.Theme.Tokens.TokenColorSemanticInfo) is { } emphasis)
        {
            text.Foreground = emphasis;
        }
        return text;
    }

    private static string Plain(IReadOnlyList<MarkdownInline> inlines)
    {
        System.Text.StringBuilder text = new();
        foreach (MarkdownInline inline in inlines)
        {
            switch (inline)
            {
                case MarkdownText literal:
                    text.Append(literal.Text);
                    break;
                case MarkdownCode code:
                    text.Append(code.Text);
                    break;
                case MarkdownLink link:
                    text.Append(link.Text);
                    break;
                case MarkdownEmphasis emphasis:
                    text.Append(Plain(emphasis.Children));
                    break;
                case MarkdownStrong strong:
                    text.Append(Plain(strong.Children));
                    break;
                default:
                    break;
            }
        }
        return text.ToString();
    }
}
