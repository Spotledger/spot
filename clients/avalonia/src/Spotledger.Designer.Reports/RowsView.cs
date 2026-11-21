namespace Spotledger.Designer.Reports;

/// <summary>A column heading of a table the builders show.</summary>
public sealed record ColumnView(string Name, string Label, bool Masked);

/// <summary>One row of display text.</summary>
public sealed record RowView(IReadOnlyList<string> Cells);

/// <summary>
/// A bounded result as the builders show it. A masked column always shows the same mask text and
/// never a value: if a server (wrongly) sent one it is dropped here, so the client cannot leak
/// what the viewer has no right to read.
/// </summary>
public sealed class RowsView
{
    /// <summary>What a masked cell shows.</summary>
    public const string MaskText = "••••";

    /// <summary>What an empty cell shows.</summary>
    public const string EmptyText = "";

    /// <summary>An empty view.</summary>
    public static RowsView Empty { get; } = new(Array.Empty<ColumnView>(), Array.Empty<RowView>(), string.Empty);

    private RowsView(IReadOnlyList<ColumnView> columns, IReadOnlyList<RowView> rows, string note)
    {
        Columns = columns;
        Rows = rows;
        Note = note;
    }

    /// <summary>The headings.</summary>
    public IReadOnlyList<ColumnView> Columns { get; }

    /// <summary>The rows, at most the cap.</summary>
    public IReadOnlyList<RowView> Rows { get; }

    /// <summary>"Showing the first N rows" when the server cut the result, else empty.</summary>
    public string Note { get; }

    /// <summary>Whether any column is masked for this viewer.</summary>
    public bool HasMaskedColumns => Columns.Any(c => c.Masked);

    /// <summary>Builds the display form of a bounded result, never exceeding <paramref name="displayCap"/> rows.</summary>
    public static RowsView From(ReportRows rows, int displayCap)
    {
        ArgumentNullException.ThrowIfNull(rows);
        var columns = rows.Columns.Select(c => new ColumnView(c.Name, c.Label, c.Masked)).ToList();
        var cap = Math.Min(Math.Max(displayCap, 0), rows.Cap > 0 ? rows.Cap : displayCap);
        var shown = new List<RowView>();
        foreach (var row in rows.Rows.Take(cap))
        {
            var cells = new string[columns.Count];
            for (var i = 0; i < cells.Length; i++)
            {
                cells[i] = columns[i].Masked ? MaskText : (i < row.Count ? row[i] ?? EmptyText : EmptyText);
            }

            shown.Add(new RowView(cells));
        }

        var cut = rows.Truncated || rows.Rows.Count > shown.Count;
        var note = cut ? $"Showing the first {shown.Count} rows." : string.Empty;
        return new RowsView(columns, shown, note);
    }
}
