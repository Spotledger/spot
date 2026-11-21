using Avalonia.Controls;
using Avalonia.Layout;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

internal static class PickerColumns
{
    // Equal star columns align headers/cells; inherited FlowDirection mirrors the grid at render time.
    public static Grid Create(IReadOnlyList<LookupColumn> columns, LookupItem? item)
    {
        var grid = new Grid { HorizontalAlignment = HorizontalAlignment.Stretch };
        for (var i = 0; i < columns.Count; i++)
        {
            grid.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Star));
            var column = columns[i];
            var text = new TextBlock
            {
                Text = item is null ? column.Label : item.Cells?.GetValueOrDefault(column.Name) ?? string.Empty,
                TextAlignment = Avalonia.Media.TextAlignment.Start,
            };
            Grid.SetColumn(text, i);
            grid.Children.Add(text);
        }
        return grid;
    }
}
