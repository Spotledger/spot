using Avalonia.Controls;
using Avalonia.Interactivity;

namespace Spotledger.Designer.Reports.Views;

/// <summary>
/// The report builder's view. It only forwards what the person did to the view-model, which sends
/// the op to the server; a server failure lands in the status line instead of escaping.
/// </summary>
public partial class ReportBuilderView : UserControl
{
    private static readonly Compare[] AllCompares = Enum.GetValues<Compare>();

    /// <summary>Creates the view.</summary>
    public ReportBuilderView()
    {
        InitializeComponent();
        FilterOperator.ItemsSource = AllCompares;
        FilterOperator.SelectedIndex = 0;
        AggregateBox.ItemsSource = Enum.GetValues<Aggregate>();
        AggregateBox.SelectedIndex = 0;
        ChartKindBox.ItemsSource = Enum.GetValues<ChartKind>();
        ChartKindBox.SelectedIndex = 1;
        BaseSearch.TextChanged += OnBaseSearchChanged;
    }

    private ReportBuilderViewModel? Builder => DataContext as ReportBuilderViewModel;

    private FieldChoice? Selected => Palette.SelectedItem as FieldChoice;

    private DraftColumn? SelectedColumn => ColumnList.SelectedItem as DraftColumn;

    private async void OnBaseSearchChanged(object? sender, TextChangedEventArgs e)
    {
        if (Builder is { } builder)
        {
            await builder.SearchBaseAsync(BaseSearch.Text ?? string.Empty);
        }
    }

    private async void OnUseBase(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && BaseList.SelectedItem is Spotledger.Client.Designer.DocTypeChoice choice)
        {
            await builder.TryAsync(() => builder.SetBaseAsync(choice.Name));
        }
    }

    private async void OnSetLabel(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && !string.IsNullOrWhiteSpace(LabelBox.Text))
        {
            await builder.TryAsync(() => builder.SetLabelAsync(LabelBox.Text.Trim()));
        }
    }

    private async void OnAddColumn(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && Selected is { } field)
        {
            await builder.TryAsync(() => builder.AddColumnAsync(field));
        }
    }

    private async void OnGroup(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && Selected is { } field)
        {
            await builder.TryAsync(() => builder.ToggleGroupAsync(field.Path));
        }
    }

    private async void OnSortAsc(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && Selected is { } field)
        {
            await builder.TryAsync(() => builder.SortByAsync(field.Path, SortDirection.Asc));
        }
    }

    private async void OnSortDesc(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && Selected is { } field)
        {
            await builder.TryAsync(() => builder.SortByAsync(field.Path, SortDirection.Desc));
        }
    }

    private async void OnClearSort(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder)
        {
            await builder.TryAsync(builder.ClearSortAsync);
        }
    }

    private async void OnRemoveColumn(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && SelectedColumn is { } column)
        {
            await builder.TryAsync(() => builder.RemoveColumnAsync(column.Field));
        }
    }

    private async void OnSetAggregate(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && SelectedColumn is { } column && AggregateBox.SelectedItem is Aggregate aggregate)
        {
            await builder.TryAsync(() => builder.SetAggregateAsync(column.Field, aggregate));
        }
    }

    private async void OnToggleTotal(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && SelectedColumn is { } column)
        {
            await builder.TryAsync(() => builder.ToggleTotalAsync(column.Field));
        }
    }

    private async void OnSetChart(object? sender, RoutedEventArgs e)
    {
        if (Builder is not { } builder || ChartKindBox.SelectedItem is not ChartKind kind)
        {
            return;
        }

        var series = ChartSeriesList.SelectedItems?.OfType<string>().ToList() ?? new List<string>();
        await builder.TryAsync(() => builder.SetChartAsync(kind, ChartXBox.SelectedItem as string, series));
    }

    private async void OnClearChart(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder)
        {
            await builder.TryAsync(() => builder.SetChartAsync(null));
        }
    }

    private async void OnRemoveFilter(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder && FilterList.SelectedIndex >= 0)
        {
            var index = FilterList.SelectedIndex;
            await builder.TryAsync(() => builder.RemoveFilterAsync(index));
        }
    }

    private async void OnAddFilter(object? sender, RoutedEventArgs e)
    {
        if (Builder is not { } builder || Selected is not { } field)
        {
            return;
        }

        var editor = ReportBuilderViewModel.NewFilter(field);
        if (FilterOperator.SelectedItem is Compare op && editor.Operators.Contains(op))
        {
            editor.Operator = op;
        }

        editor.ValueText = FilterBox.Text ?? string.Empty;
        editor.YesNo = FilterYes.IsChecked == true;
        editor.IsParameter = FilterIsParameter.IsChecked == true;
        await builder.TryAsync(() => builder.AddFilterAsync(editor));
    }

    private async void OnShare(object? sender, RoutedEventArgs e)
    {
        if (Builder is { } builder)
        {
            var roles = (RolesBox.Text ?? string.Empty).Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries);
            await builder.TryAsync(() => builder.ShareAsync(roles));
        }
    }
}
