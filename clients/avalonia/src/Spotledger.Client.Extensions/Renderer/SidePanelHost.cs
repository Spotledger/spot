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
/// The side-panel surface (<c>doc.sidepanel</c>, <c>list.sidepanel</c>): one card per
/// visible panel, its blocks in the document's order. Data blocks draw their loading, empty
/// and error-with-retry states; every visual value is a theme token, spacing follows the
/// density, and the reading order is the tab order so a keyboard user reaches every link,
/// retry and row action.
/// </summary>
public sealed class SidePanelHost : UserControl
{
    private readonly StackPanel _body = new() { Orientation = Orientation.Vertical };
    private SlotView? _view;

    /// <summary>Creates the host.</summary>
    public SidePanelHost()
    {
        Content = _body;
        HorizontalAlignment = HorizontalAlignment.Stretch;
        Loaded += (_, _) => ApplyTheme();
        ActualThemeVariantChanged += (_, _) => ApplyTheme();
    }

    /// <summary>The view this host draws, or null.</summary>
    public SlotView? View => _view;

    /// <summary>The visible panel cards, in order (tests).</summary>
    public IReadOnlyList<Control> Cards => [.. _body.Children.OfType<Control>()];

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
        _body.Children.Clear();
        _body.Spacing = ThemeAccess.FieldGap(this);
        if (_view is null)
        {
            return;
        }
        int tabIndex = 0;
        foreach (SlotEntry entry in _view.Entries)
        {
            if (entry is PanelModel panel)
            {
                _body.Children.Add(Card(panel, ref tabIndex));
            }
        }
    }

    private Border Card(PanelModel panel, ref int tabIndex)
    {
        StackPanel content = new() { Orientation = Orientation.Vertical, Spacing = ThemeAccess.FieldGap(this) };
        TextBlock heading = new() { Text = panel.Label };
        ThemeAccess.Heading(heading, this);
        ThemeAccess.ApplyScriptFont(heading, this, panel.Label);
        AutomationProperties.SetName(content, panel.Label);
        content.Children.Add(heading);
        foreach (PanelBlockView block in panel.Blocks)
        {
            content.Children.Add(BlockView(block, panel, ref tabIndex));
        }
        return new Border
        {
            Background = ThemeAccess.Brush(this, Tokens.TokenColorSurfaceRaised),
            BorderThickness = new Thickness(1),
            BorderBrush = ThemeAccess.Brush(this, Tokens.TokenColorBorderDefault),
            CornerRadius = new CornerRadius(ThemeAccess.Number(this, Tokens.TokenRadiusMedium, 6)),
            Padding = new Thickness(ThemeAccess.FieldGap(this)),
            Child = content
        };
    }

    /// <summary>The loading state as themed text (the framework's progress control has no
    /// theme in the client's theme set, so it would draw nothing).</summary>
    private TextBlock Loading()
    {
        TextBlock text = new() { Text = "Loading\u2026" };
        ThemeAccess.Caption(text, this);
        return text;
    }

    private Control BlockView(PanelBlockView block, PanelModel panel, ref int tabIndex)
    {
        switch (block)
        {
            case FieldListBlockView fieldList:
                return Entries(fieldList.Items, ref tabIndex);
            case KeyValueBlockView keyValue:
                return Entries(keyValue.Items, ref tabIndex);
            case TextBlockView text:
                {
                    MarkdownPresenter presenter = new();
                    presenter.Show(text.Model, (link, token) => OpenLinkAsync(panel, link, token));
                    return presenter;
                }
            case TableBlockView table:
                return Table(table, panel, ref tabIndex);
            case StatBlockView stat:
                return Stat(stat, ref tabIndex);
            case ListOfLinksBlockView links:
                return Links(links, ref tabIndex);
            case EmptyStateBlockView empty:
                {
                    TextBlock text = new() { Text = empty.Text };
                    ThemeAccess.Caption(text, this);
                    return text;
                }
            default:
                return new Panel();
        }
    }

    private Grid Entries(IReadOnlyList<BlockEntry> entries, ref int tabIndex)
    {
        _ = tabIndex;
        Grid grid = new() { ColumnDefinitions = new ColumnDefinitions("Auto,*") };
        int row = 0;
        foreach (BlockEntry entry in entries)
        {
            TextBlock label = new() { Text = entry.Label, Margin = new Thickness(0, 0, ThemeAccess.FieldGap(this), 0) };
            ThemeAccess.Caption(label, this);
            ThemeAccess.ApplyScriptFont(label, this, entry.Label);
            Grid.SetColumn(label, 0);
            Grid.SetRow(label, row);
            grid.Children.Add(label);

            TextBlock value = new() { Text = entry.Text };
            ThemeAccess.Body(value, this);
            ThemeAccess.ApplyScriptFont(value, this, entry.Text);
            Grid.SetColumn(value, 1);
            Grid.SetRow(value, row);
            grid.RowDefinitions.Add(new RowDefinition(GridLength.Auto));
            grid.Children.Add(value);
            row++;
        }
        return grid;
    }

    private StackPanel Table(TableBlockView table, PanelModel panel, ref int tabIndex)
    {
        StackPanel body = new() { Orientation = Orientation.Vertical, Spacing = ThemeAccess.FieldGap(this) / 2 };
        switch (table.State)
        {
            case PanelDataState.Error:
                {
                    TextBlock problem = new() { Text = table.Problem ?? "The query failed." };
                    ThemeAccess.Caption(problem, this);
                    SpotButton retry = new() { Variant = SpotButtonVariant.Secondary, Content = "Retry", TabIndex = tabIndex++ };
                    AutomationProperties.SetName(retry, $"Retry {table.Query}");
                    TableBlockView target = table;
                    retry.Click += async (_, _) => await target.RetryAsync(CancellationToken.None).ConfigureAwait(true);
                    body.Children.Add(problem);
                    body.Children.Add(retry);
                    return body;
                }
            case PanelDataState.Loading:
                body.Children.Add(Loading());
                return body;
            case PanelDataState.Empty:
                {
                    TextBlock empty = new() { Text = panel.EmptyText };
                    ThemeAccess.Caption(empty, this);
                    body.Children.Add(empty);
                    return body;
                }
            default:
                break;
        }
        Grid grid = new() { ColumnDefinitions = new ColumnDefinitions(string.Join(',', Enumerable.Repeat("*", Math.Max(1, table.ColumnLabels.Count)))) };
        for (int column = 0; column < table.ColumnLabels.Count; column++)
        {
            TextBlock header = new() { Text = table.ColumnLabels[column] };
            ThemeAccess.Caption(header, this);
            header.FontWeight = ThemeAccess.Weight(this, Tokens.TokenFontWeightSemibold, FontWeight.SemiBold);
            header.Margin = new Thickness(ThemeAccess.CellPadding(this));
            Grid.SetColumn(header, column);
            Grid.SetRow(header, 0);
            grid.Children.Add(header);
        }
        grid.RowDefinitions.Add(new RowDefinition(GridLength.Auto));
        for (int row = 0; row < table.Rows.Count; row++)
        {
            grid.RowDefinitions.Add(new RowDefinition(GridLength.Auto));
            for (int column = 0; column < table.Rows[row].Count; column++)
            {
                TextBlock cell = new() { Text = table.Rows[row][column] };
                ThemeAccess.Body(cell, this);
                cell.Margin = new Thickness(ThemeAccess.CellPadding(this));
                Grid.SetColumn(cell, column);
                Grid.SetRow(cell, row + 1);
                grid.Children.Add(cell);
            }
        }
        body.Children.Add(grid);
        if (table.HasMore)
        {
            SpotButton more = new() { Variant = SpotButtonVariant.Subtle, Content = "More", TabIndex = tabIndex++ };
            AutomationProperties.SetName(more, $"More rows of {table.Query}");
            TableBlockView target = table;
            more.Click += async (_, _) => await target.LoadMoreAsync(CancellationToken.None).ConfigureAwait(true);
            body.Children.Add(more);
        }
        return body;
    }

    private StackPanel Stat(StatBlockView stat, ref int tabIndex)
    {
        StackPanel body = new() { Orientation = Orientation.Vertical };
        TextBlock label = new() { Text = stat.Label };
        ThemeAccess.Caption(label, this);
        ThemeAccess.ApplyScriptFont(label, this, stat.Label);
        body.Children.Add(label);
        switch (stat.State)
        {
            case PanelDataState.Error:
                {
                    TextBlock problem = new() { Text = stat.Problem ?? "The query failed." };
                    ThemeAccess.Caption(problem, this);
                    SpotButton retry = new() { Variant = SpotButtonVariant.Secondary, Content = "Retry", TabIndex = tabIndex++ };
                    AutomationProperties.SetName(retry, $"Retry {stat.Query}");
                    StatBlockView target = stat;
                    retry.Click += async (_, _) => await target.RetryAsync(CancellationToken.None).ConfigureAwait(true);
                    body.Children.Add(problem);
                    body.Children.Add(retry);
                    break;
                }
            case PanelDataState.Loading:
                body.Children.Add(Loading());
                break;
            default:
                {
                    TextBlock value = new() { Text = stat.Text };
                    ThemeAccess.Display(value, this);
                    AutomationProperties.SetName(value, $"{stat.Label}: {stat.Text}");
                    body.Children.Add(value);
                    break;
                }
        }
        return body;
    }

    private StackPanel Links(ListOfLinksBlockView links, ref int tabIndex)
    {
        StackPanel body = new() { Orientation = Orientation.Vertical, Spacing = ThemeAccess.FieldGap(this) / 2 };
        foreach (LinkItem link in links.Items)
        {
            SpotButton button = new() { Variant = SpotButtonVariant.Subtle, Content = link.Label, TabIndex = tabIndex++, HorizontalAlignment = HorizontalAlignment.Stretch };
            AutomationProperties.SetName(button, link.Label);
            LinkItem target = link;
            button.Click += async (_, _) => await links.OpenAsync(target, CancellationToken.None).ConfigureAwait(true);
            body.Children.Add(button);
        }
        return body;
    }

    private async Task OpenLinkAsync(PanelModel panel, MarkdownLink link, CancellationToken cancellationToken)
    {
        if (_view?.Services.Shell is not { } shell)
        {
            return;
        }
        _ = panel;
        await shell.OpenAsync(link.Destination, [], cancellationToken).ConfigureAwait(true);
    }
}
