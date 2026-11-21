using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Extensions;
using Spotledger.Client.Theme;
using TextBlock = Avalonia.Controls.TextBlock;

namespace Spotledger.Client.Extensions.Renderer;

/// <summary>
/// The home-tile surface (<c>home.tiles</c>): one tile card per visible tile, the
/// composite drawn by <see cref="ComponentPresenter"/>. A tile with an <c>open</c> is a
/// button (keyboard reachable, accessible name from its label); a tile whose aggregate
/// failed shows its problem and a retry, never a blank card.
/// </summary>
public sealed class TileHost : UserControl
{
    private readonly WrapPanel _tiles = new() { Orientation = Orientation.Horizontal };
    private SlotView? _view;

    /// <summary>Creates the host.</summary>
    public TileHost()
    {
        Content = _tiles;
        HorizontalAlignment = HorizontalAlignment.Stretch;
        Loaded += (_, _) => ApplyTheme();
        ActualThemeVariantChanged += (_, _) => ApplyTheme();
    }

    /// <summary>The view this host draws, or null.</summary>
    public SlotView? View => _view;

    /// <summary>The drawn tiles, in order (tests).</summary>
    public IReadOnlyList<Control> Tiles => [.. _tiles.Children.OfType<Control>()];

    /// <summary>Shows one slot view and subscribes to its changes.</summary>
    public void Show(SlotView view)
    {
        ArgumentNullException.ThrowIfNull(view);
        if (_view is not null)
        {
            _view.Changed -= OnChanged;
        }
        _view = view;
        _view.Changed += OnChanged;
        Rebuild();
    }

    /// <summary>Rebuilds after a theme or density change.</summary>
    public void ApplyTheme() => Rebuild();


    /// <inheritdoc/>
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == Density.ModeProperty)
        {
            Rebuild();
        }
    }

    private void OnChanged(object? sender, EventArgs e) => Rebuild();

    private void Rebuild()
    {
        _tiles.Children.Clear();
        double gap = ThemeAccess.FieldGap(this);
        if (_view is null)
        {
            return;
        }
        foreach (SlotEntry entry in _view.Entries)
        {
            if (entry is TileModel tile)
            {
                Control built = Tile(tile);
                built.Margin = new Thickness(0, 0, gap, gap);
                _tiles.Children.Add(built);
            }
        }
    }

    private Control Tile(TileModel tile)
    {
        ComponentPresenter presenter = new();
        presenter.Show(tile.Root);
        StackPanel content = new() { Orientation = Orientation.Vertical, Spacing = ThemeAccess.FieldGap(this), Children = { presenter } };
        TextBlock label = new() { Text = tile.Label };
        ThemeAccess.Heading(label, this);
        ThemeAccess.ApplyScriptFont(label, this, tile.Label);
        content.Children.Insert(0, label);
        if (tile.State == PanelDataState.Error)
        {
            TextBlock problem = new() { Text = tile.Problem ?? "The tile's query failed." };
            ThemeAccess.Caption(problem, this);
            SpotButton retry = new() { Variant = SpotButtonVariant.Secondary, Content = "Retry" };
            AutomationProperties.SetName(retry, $"Retry {tile.Label}");
            TileModel target = tile;
            retry.Click += async (_, _) => await target.LoadAsync(CancellationToken.None).ConfigureAwait(true);
            content.Children.Add(problem);
            content.Children.Add(retry);
        }
        Border card = new()
        {
            Background = ThemeAccess.Brush(this, Tokens.TokenColorSurfaceRaised),
            BorderThickness = new Thickness(1),
            BorderBrush = ThemeAccess.Brush(this, Tokens.TokenColorBorderDefault),
            CornerRadius = new CornerRadius(ThemeAccess.Number(this, Tokens.TokenRadiusLarge, 8)),
            Padding = new Thickness(ThemeAccess.FieldGap(this)),
            MinWidth = ThemeAccess.Number(this, Tokens.TokenSpace12, 48) * 4,
            Child = content
        };
        if (!tile.OpensOnClick)
        {
            return card;
        }
        SpotButton button = new()
        {
            Variant = SpotButtonVariant.Subtle,
            Content = card,
            HorizontalContentAlignment = HorizontalAlignment.Stretch,
            Padding = new Thickness(0)
        };
        AutomationProperties.SetName(button, tile.Label);
        AutomationProperties.SetAutomationId(button, tile.Id);
        button.Click += async (_, _) => await tile.OpenAsync(CancellationToken.None).ConfigureAwait(true);
        return button;
    }
}
