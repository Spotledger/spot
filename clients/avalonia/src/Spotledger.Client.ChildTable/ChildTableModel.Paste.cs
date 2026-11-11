using Spotledger.Client.Metadata;

namespace Spotledger.Client.ChildTable;

public sealed partial class ChildTableModel
{
    /// <summary>
    /// Pastes tab-separated text. The first cell lands on (<paramref name="startRow"/>,
    /// <paramref name="startColumn"/>); rows below the end of the table become new rows, so a
    /// paste overwrites from the cursor and appends what does not fit. Each cell is parsed by its
    /// field's own type. The paste is all or nothing: if any cell cannot be applied (it does not
    /// parse, its cell is read-only, it lies past the last column) nothing is written and
    /// <see cref="PasteReport.Errors"/> lists every such cell. One paste is one undo step.
    /// </summary>
    /// <param name="text">The clipboard text.</param>
    /// <param name="startRow">The 0-based row of the first cell; the row count (or more) appends.</param>
    /// <param name="startColumn">The column of the first cell.</param>
    public ChildTableResult Paste(string text, int startRow, int startColumn)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (Gate() is { } refused)
        {
            return refused;
        }

        var matrix = ChildTableTsv.Parse(text);
        if (matrix.Count == 0)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "The clipboard holds no text.");
        }

        if (_columns.Count == 0)
        {
            return ChildTableResult.Refused(ChildTableRefusal.OutOfRange);
        }

        var errors = new List<PasteCellError>();
        if (matrix.Count > MaxPasteRows)
        {
            errors.Add(new PasteCellError(0, null, string.Empty, $"At most {MaxPasteRows} rows can be pasted at once."));
            return Invalid(errors);
        }

        startRow = Math.Clamp(startRow, 0, Table.Rows.Count);
        startColumn = Math.Clamp(startColumn, 0, _columns.Count - 1);

        // Phase 1, no change: parse every cell and check every cell that already exists.
        var parsed = new List<(int Row, int Column, FieldValue Value)>();
        for (var r = 0; r < matrix.Count; r++)
        {
            var rowIndex = startRow + r;
            var exists = rowIndex < Table.Rows.Count;
            for (var c = 0; c < matrix[r].Length; c++)
            {
                var columnIndex = startColumn + c;
                var cellText = matrix[r][c];
                if (columnIndex >= _columns.Count)
                {
                    errors.Add(new PasteCellError(rowIndex + 1, null, cellText, "There is no column for this cell."));
                    continue;
                }

                var field = _columns[columnIndex].Field;
                var refusal = exists
                    ? CellRefusal(rowIndex, columnIndex)
                    : IsInlineEditable(field) && !field.ReadOnly ? ChildTableRefusal.None : ChildTableRefusal.CellKindNotEditable;
                if (refusal != ChildTableRefusal.None)
                {
                    // An empty cell into a cell that holds nothing and cannot be edited writes nothing,
                    // so nothing is dropped: a copy of this table leaves such cells empty.
                    if (!(cellText.Length == 0 && (!exists || CurrentText(rowIndex, columnIndex).Length == 0)))
                    {
                        errors.Add(new PasteCellError(rowIndex + 1, field.Name, cellText, ChildTableResult.Describe(refusal)));
                    }

                    continue;
                }

                var input = field.Kind is FieldKind.Text ? cellText : cellText.Trim();
                if (!field.TryParse(input, out var value, out var error, NumberParsingSettings.WithSeparators))
                {
                    errors.Add(new PasteCellError(rowIndex + 1, field.Name, cellText, error.Message));
                    continue;
                }

                if (exists && string.Equals(CurrentText(rowIndex, columnIndex), CellFormatter.Format(field, value), StringComparison.Ordinal)
                    && Table.Rows[rowIndex].Values[field.Name].Value is var current && SameKind(current, value!))
                {
                    continue; // already holds this value: nothing to write, nothing to journal
                }

                parsed.Add((rowIndex, columnIndex, value!));
            }
        }

        if (errors.Count > 0)
        {
            return Invalid(errors);
        }

        // Phase 2: write everything as one journaled operation.
        var created = 0;
        var written = 0;
        var skipped = new List<PasteCellError>();
        var lastRow = startRow + matrix.Count - 1;
        using (Table.Begin($"paste {matrix.Count} row{(matrix.Count == 1 ? string.Empty : "s")}"))
        {
            while (Table.Rows.Count <= lastRow)
            {
                Table.AddRow();
                created++;
            }

            foreach (var (row, column, value) in parsed)
            {
                var field = _columns[column].Field;
                var refusal = FieldRefusal(Table.Rows[row], field);
                if (refusal != ChildTableRefusal.None)
                {
                    // A row that did not exist when the paste was checked can carry a permission
                    // the check could not see; the cell is listed, never written, never silent.
                    skipped.Add(new PasteCellError(row + 1, field.Name, CellFormatter.Format(field, value), ChildTableResult.Describe(refusal)));
                    continue;
                }

                Table.Rows[row].SetValue(field.Name, value);
                written++;
            }
        }

        var report = new PasteReport(true, created, written, skipped);
        return ChildTableResult.Ok((Math.Min(lastRow, Table.Rows.Count - 1), startColumn), Enumerable.Range(startRow, lastRow - startRow + 1).ToList(), report);
    }

    private static bool SameKind(FieldValue a, FieldValue b) => a.GetType() == b.GetType();

    private string CurrentText(int row, int column)
    {
        var field = _columns[column].Field;
        return Table.Rows[row].Values.TryGetValue(field.Name, out var state) && !state.IsMasked
            ? CellFormatter.Format(field, state.Value)
            : string.Empty;
    }

    private static ChildTableResult Invalid(List<PasteCellError> errors)
    {
        var report = new PasteReport(false, 0, 0, errors);
        var first = errors[0];
        var message = errors.Count == 1
            ? $"Nothing was pasted. Row {first.Row}: {first.Message}"
            : $"Nothing was pasted. {errors.Count} cells could not be applied; the first is row {first.Row}: {first.Message}";
        return ChildTableResult.Refused(ChildTableRefusal.Invalid, message, report);
    }
}
