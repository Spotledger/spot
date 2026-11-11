using Spotledger.Client.Grid;

namespace Spotledger.Client.Grid.Gallery;

/// <summary>
/// A tiny sample page the Desktop dev menu can host (not the product shell): a grid
/// over 1,000 synthetic rows demonstrating selection, sorting, resizing, density and
/// the footer. Lives inside the Grid project so the gallery carries no Api/Metadata
/// reference and no Desktop dependency.
/// </summary>
public static class GridGalleryPage
{
    /// <summary>Builds the sample grid with gallery fixtures (1,000 × 6) and demo
    /// behaviours wired (sort, footer, check column on the first column).</summary>
    public static SpotGrid BuildSample()
    {
        var rows = GridFixture.MakeRows(1000, 6);
        var source = new InMemoryGridSource(rows, row => row.Cells[0].Text);
        var columns = new List<GridColumnModel>
        {
            new() { Header = "Item", Width = GridColumnWidth.Pixels(180) },
            new() { Header = "Check", Width = GridColumnWidth.Pixels(48), IsCheckColumn = true, IsSortable = false, IsResizable = false },
            new() { Header = "Category", Width = GridColumnWidth.Star(1) },
            new() { Header = "Quantity", Width = GridColumnWidth.Pixels(90), DefaultAlignment = GridCellAlignment.End },
            new() { Header = "Amount", Width = GridColumnWidth.Star(1), DefaultAlignment = GridCellAlignment.End },
            new() { Header = "Status", Width = GridColumnWidth.Pixels(110) },
        };

        return new SpotGrid
        {
            Source = source,
            Columns = columns,
            Footer = GridRow.Of(
                "summary",
                new GridCell("Summary"),
                new GridCell(string.Empty),
                new GridCell("1,000 items"),
                new GridCell(string.Empty),
                new GridCell(string.Empty),
                new GridCell(string.Empty)),
        };
    }
}