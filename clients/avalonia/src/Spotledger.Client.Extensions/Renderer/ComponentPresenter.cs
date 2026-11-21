using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Media.TextFormatting;
using Spotledger.Client.Controls;
using Spotledger.Client.Extensions;
using TextBlock = Avalonia.Controls.TextBlock;

namespace Spotledger.Client.Extensions.Renderer;

/// <summary>
/// Draws one expanded composite: layout primitives map to panels, catalog presentations to
/// text roles and the catalog's button, and a control the catalogue does not have to a
/// visible placeholder. Every visual value is a theme token; the presenter holds no app
/// code and never evaluates anything itself — the model already did.
/// </summary>
public sealed class ComponentPresenter : UserControl
{
    private ExpandedNode? _root;

    /// <summary>Creates the presenter.</summary>
    public ComponentPresenter()
    {
        HorizontalContentAlignment = HorizontalAlignment.Stretch;
        VerticalContentAlignment = VerticalAlignment.Top;
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

    /// <summary>The tree to draw, or null for nothing.</summary>
    public void Show(ExpandedNode? root)
    {
        _root = root;
        Content = root is null ? null : Build(root, 0);
    }

    /// <summary>The root currently drawn (tests and accessible names).</summary>
    public ExpandedNode? Root => _root;

    /// <summary>Rebuilds the tree after a theme or density change.</summary>
    public void ApplyTheme() => Content = _root is null ? null : Build(_root, 0);

    private Control Build(ExpandedNode node, int depth)
    {
        ArgumentNullException.ThrowIfNull(node);
        return node.Kind switch
        {
            ExpandedNodeKind.Row => Layout(node, Orientation.Horizontal, depth),
            ExpandedNodeKind.Column => Layout(node, Orientation.Vertical, depth),
            ExpandedNodeKind.Grid => Grid(node, depth),
            ExpandedNodeKind.Group => Group(node, depth),
            ExpandedNodeKind.Separator => Separator(),
            ExpandedNodeKind.Heading => Text(node.Text, ThemeAccess.Heading),
            ExpandedNodeKind.Label => Text(node.Text, ThemeAccess.Body),
            ExpandedNodeKind.Badge => Badge(node.Text),
            ExpandedNodeKind.Progress => Progress(node.Percent),
            ExpandedNodeKind.Number or ExpandedNodeKind.Currency or ExpandedNodeKind.Date => Text(node.Value, ThemeAccess.Body),
            ExpandedNodeKind.Button => Button(node),
            _ => Placeholder(node)
        };
    }

    private StackPanel Layout(ExpandedNode node, Orientation orientation, int depth)
    {
        StackPanel panel = new()
        {
            Orientation = orientation,
            Spacing = ThemeAccess.FieldGap(this),
            HorizontalAlignment = HorizontalAlignment.Stretch
        };
        foreach (ExpandedNode child in node.Children)
        {
            panel.Children.Add(Build(child, depth + 1));
        }
        return panel;
    }

    private Avalonia.Controls.Primitives.UniformGrid Grid(ExpandedNode node, int depth)
    {
        int columns = node.Columns > 0 ? node.Columns : 1;
        Avalonia.Controls.Primitives.UniformGrid grid = new() { Columns = columns, Rows = 0 };
        foreach (ExpandedNode child in node.Children)
        {
            Control built = Build(child, depth + 1);
            built.Margin = new Thickness(ThemeAccess.CellPadding(this));
            grid.Children.Add(built);
        }
        return grid;
    }

    private Border Group(ExpandedNode node, int depth)
    {
        StackPanel inner = new() { Orientation = Orientation.Vertical, Spacing = ThemeAccess.FieldGap(this) };
        if (node.Title is { Length: > 0 } title)
        {
            TextBlock heading = new() { Text = title };
            ThemeAccess.Heading(heading, this);
            ThemeAccess.ApplyScriptFont(heading, this, title);
            inner.Children.Add(heading);
        }
        foreach (ExpandedNode child in node.Children)
        {
            inner.Children.Add(Build(child, depth + 1));
        }
        return new Border
        {
            BorderThickness = new Thickness(1),
            BorderBrush = ThemeAccess.Brush(this, Spotledger.Client.Theme.Tokens.TokenColorBorderDefault),
            CornerRadius = new CornerRadius(ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenRadiusMedium, 6)),
            Padding = new Thickness(ThemeAccess.FieldGap(this)),
            Child = inner
        };
    }

    private Border Separator() => new Border
    {
        Height = 1,
        Background = ThemeAccess.Brush(this, Spotledger.Client.Theme.Tokens.TokenColorBorderDefault),
        HorizontalAlignment = HorizontalAlignment.Stretch
    };

    private TextBlock Text(string? text, Action<TextBlock, Visual> role)
    {
        TextBlock block = new() { Text = text ?? string.Empty };
        role(block, this);
        ThemeAccess.ApplyScriptFont(block, this, text ?? string.Empty);
        return block;
    }

    private Border Badge(string? text)
    {
        TextBlock label = new() { Text = text ?? string.Empty };
        ThemeAccess.Caption(label, this);
        ThemeAccess.ApplyScriptFont(label, this, text ?? string.Empty);
        return new Border
        {
            Background = ThemeAccess.Brush(this, Spotledger.Client.Theme.Tokens.TokenColorBrand100),
            CornerRadius = new CornerRadius(ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenRadiusPill, 12)),
            Padding = new Thickness(ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenSpace2, 8), 0),
            Child = label,
            HorizontalAlignment = HorizontalAlignment.Left
        };
    }

    /// <summary>A progress bar drawn from two theme-token borders rather than the unthemed
    /// framework control (the client's theme set is the design system's, not Fluent's): a
    /// sunken track and a brand fill whose width is the percentage, with the height from a
    /// spacing token, so the bar is visible in every variant.</summary>
    private Border Progress(int percent)
    {
        int filled = Math.Clamp(percent, 0, 100);
        Avalonia.Controls.Grid track = new()
        {
            ColumnDefinitions = new ColumnDefinitions(FormattableString.Invariant($"{filled}*,{100 - filled}*")),
            Height = ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenSpace1, 4),
            HorizontalAlignment = HorizontalAlignment.Stretch
        };
        Border fill = new()
        {
            // No pill radius here: a corner radius larger than the bar's height collapses the
            // rounded rectangle and the fill would not paint at all.
            Background = ThemeAccess.Brush(this, Spotledger.Client.Theme.Tokens.TokenColorBrand500),
            CornerRadius = new CornerRadius(ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenRadiusSmall, 2))
        };
        Avalonia.Controls.Grid.SetColumn(fill, 0);
        track.Children.Add(fill);
        return new Border
        {
            Background = ThemeAccess.Brush(this, Spotledger.Client.Theme.Tokens.TokenColorSurfaceSunken),
            CornerRadius = new CornerRadius(ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenRadiusSmall, 2)),
            Child = track,
            HorizontalAlignment = HorizontalAlignment.Stretch
        };
    }

    private static SpotButton Button(ExpandedNode node)
    {
        SpotButton button = new() { Variant = SpotButtonVariant.Secondary, Content = node.Text ?? string.Empty };
        AutomationProperties.SetName(button, node.Text ?? string.Empty);
        if (node.Action is { } action)
        {
            button.IsEnabled = action.IsEnabled;
            button.IsBusy = action.IsBusy;
            action.Changed += (_, _) =>
            {
                button.IsEnabled = action.IsEnabled;
                button.IsBusy = action.IsBusy;
            };
            button.Click += async (_, _) => await action.InvokeAsync(CancellationToken.None).ConfigureAwait(true);
        }
        else
        {
            button.IsEnabled = false;
        }
        return button;
    }

    private Border Placeholder(ExpandedNode node)
    {
        TextBlock text = new() { Text = node.Text ?? node.Placeholder ?? "unsupported" };
        ThemeAccess.Caption(text, this);
        return new Border
        {
            BorderThickness = new Thickness(1),
            BorderBrush = ThemeAccess.Brush(this, Spotledger.Client.Theme.Tokens.TokenColorSemanticWarning),
            CornerRadius = new CornerRadius(ThemeAccess.Number(this, Spotledger.Client.Theme.Tokens.TokenRadiusSmall, 4)),
            Padding = new Thickness(ThemeAccess.CellPadding(this)),
            Child = text
        };
    }
}
