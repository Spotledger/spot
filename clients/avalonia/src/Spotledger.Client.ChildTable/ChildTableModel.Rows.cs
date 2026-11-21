using Spotledger.Client.Metadata;
using RowDraft = Spotledger.Client.Metadata.DocumentDraft.TableDraft.RowDraft;

namespace Spotledger.Client.ChildTable;

public sealed partial class ChildTableModel
{
    /// <summary>The most rows one paste may create or overwrite.</summary>
    public const int MaxPasteRows = 2000;

    private ChildTableResult? Gate() => CheckEditable();

    private List<int>? ValidRows(IEnumerable<int> rows)
    {
        var list = rows.Distinct().Order().ToList();
        return list.Count == 0 || list[0] < 0 || list[^1] >= Table.Rows.Count ? null : list;
    }

    /// <summary>Sets one cell. The only way a cell value changes through this class: it goes
    /// through the row's <c>SetValue</c> so dirty tracking, the undo journal, the rules and the
    /// server-error clearing stay in one place.</summary>
    public ChildTableResult SetCell(int rowIndex, int column, FieldValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        var refusal = CellRefusal(rowIndex, column);
        if (refusal != ChildTableRefusal.None)
        {
            return ChildTableResult.Refused(refusal);
        }

        Table.Rows[rowIndex].SetValue(_columns[column].Field.Name, value);
        return ChildTableResult.Ok();
    }

    /// <summary>Appends a row.</summary>
    public ChildTableResult AddRow()
    {
        if (Gate() is { } refused)
        {
            return refused;
        }

        Table.AddRow();
        var index = Table.Rows.Count - 1;
        return ChildTableResult.Ok((index, FirstEditableColumn(index)), [index]);
    }

    /// <summary>Inserts a row so that it sits at <paramref name="index"/> (rows from there on
    /// move down).</summary>
    public ChildTableResult InsertRow(int index)
    {
        if (Gate() is { } refused)
        {
            return refused;
        }

        index = Math.Clamp(index, 0, Table.Rows.Count);
        Table.AddRowAt(index);
        return ChildTableResult.Ok((index, FirstEditableColumn(index)), [index]);
    }

    /// <summary>Deletes rows as one undoable operation.</summary>
    public ChildTableResult DeleteRows(IEnumerable<int> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        if (Gate() is { } refused)
        {
            return refused;
        }

        var list = ValidRows(rows);
        if (list is null)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "No row to delete.");
        }

        if (list.Count == 1)
        {
            Table.RemoveRowAt(list[0]);
        }
        else
        {
            using var operation = Table.Begin($"delete {list.Count} rows");
            for (var i = list.Count - 1; i >= 0; i--)
            {
                Table.RemoveRowAt(list[i]);
            }
        }

        if (Table.Rows.Count == 0)
        {
            return ChildTableResult.Ok(selection: []);
        }

        var next = Math.Min(list[0], Table.Rows.Count - 1);
        return ChildTableResult.Ok((next, -1), [next]);
    }

    /// <summary>Moves rows one place up (<paramref name="delta"/> -1) or down (+1), keeping their
    /// order and identity; a row that cannot move stays and blocks the rows against it. The result's
    /// cursor follows <paramref name="cursorRow"/> to where it ended up.</summary>
    public ChildTableResult MoveRows(IEnumerable<int> rows, int delta, int cursorRow = -1)
    {
        ArgumentNullException.ThrowIfNull(rows);
        if (delta is not (-1 or 1))
        {
            throw new ArgumentOutOfRangeException(nameof(delta), delta, "Rows move one place at a time.");
        }

        if (Gate() is { } refused)
        {
            return refused;
        }

        var list = ValidRows(rows);
        if (list is null)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "No row to move.");
        }

        var count = Table.Rows.Count;
        var final = new Dictionary<int, int>();
        var blocked = new HashSet<int>();
        var plan = new List<(int From, int To)>();
        var ordered = delta < 0 ? list : Enumerable.Reverse(list).ToList();
        foreach (var index in ordered)
        {
            var target = index + delta;
            if (target < 0 || target >= count || blocked.Contains(target))
            {
                blocked.Add(index);
                final[index] = index;
            }
            else
            {
                plan.Add((index, target));
                final[index] = target;
            }
        }

        if (plan.Count == 0)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "The rows cannot move further.");
        }

        if (plan.Count == 1)
        {
            Table.MoveRow(plan[0].From, plan[0].To);
        }
        else
        {
            using var operation = Table.Begin($"move {plan.Count} rows");
            foreach (var (from, to) in plan)
            {
                Table.MoveRow(from, to);
            }
        }

        var selection = final.Values.Order().ToList();
        (int Row, int Column)? cursor = final.TryGetValue(cursorRow, out var landed) ? (landed, -1) : null;
        return ChildTableResult.Ok(cursor, selection);
    }

    /// <summary>Duplicates rows: the copies sit together under the last of the rows, in order,
    /// as one undoable operation. Only editable values are copied; computed, read-only and masked
    /// cells are left to the new row's rules.</summary>
    public ChildTableResult DuplicateRows(IEnumerable<int> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        if (Gate() is { } refused)
        {
            return refused;
        }

        var list = ValidRows(rows);
        if (list is null)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "No row to duplicate.");
        }

        var sources = list.Select(index => Table.Rows[index]).ToList();
        var at = list[^1] + 1;
        var created = new List<int>();
        using (Table.Begin($"duplicate {list.Count} row{(list.Count == 1 ? string.Empty : "s")}"))
        {
            foreach (var source in sources)
            {
                var copy = Table.AddRowAt(at + created.Count);
                created.Add(at + created.Count);
                CopyEditableValues(source, copy);
            }
        }

        return ChildTableResult.Ok((created[0], -1), created);
    }

    private void CopyEditableValues(RowDraft source, RowDraft copy)
    {
        foreach (var column in _columns)
        {
            var name = column.Field.Name;
            if (FieldRefusal(source, column.Field) != ChildTableRefusal.None
                || FieldRefusal(copy, column.Field) != ChildTableRefusal.None
                || !source.Values.TryGetValue(name, out var state) || state.Value.IsNull)
            {
                continue;
            }

            copy.SetValue(name, state.Value);
        }
    }

    /// <summary>Fills a column down. With two or more rows the first of them is copied to the
    /// others; with one row the row above is copied into it. Refused whole when any target cell
    /// cannot be edited.</summary>
    public ChildTableResult FillDown(IEnumerable<int> rows, int column)
    {
        ArgumentNullException.ThrowIfNull(rows);
        if (Gate() is { } refused)
        {
            return refused;
        }

        var list = ValidRows(rows);
        if (list is null || column < 0 || column >= _columns.Count)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "No cell to fill.");
        }

        int source;
        List<int> targets;
        if (list.Count >= 2)
        {
            source = list[0];
            targets = list.Skip(1).ToList();
        }
        else if (list[0] > 0)
        {
            source = list[0] - 1;
            targets = [list[0]];
        }
        else
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "There is no row above to copy from.");
        }

        var field = _columns[column].Field;
        foreach (var target in targets)
        {
            var refusal = CellRefusal(target, column);
            if (refusal != ChildTableRefusal.None)
            {
                return ChildTableResult.Refused(
                    refusal,
                    $"Row {target + 1}, {field.Label}: {ChildTableResult.Describe(refusal)} Nothing was filled.");
            }
        }

        if (!Table.Rows[source].Values.TryGetValue(field.Name, out var sourceState) || sourceState.IsMasked)
        {
            return ChildTableResult.Refused(ChildTableRefusal.CellMasked, "The cell to copy from is masked.");
        }

        var value = sourceState.Value;
        using (Table.Begin($"fill down {field.Label}"))
        {
            foreach (var target in targets)
            {
                Table.Rows[target].SetValue(field.Name, value);
            }
        }

        return ChildTableResult.Ok();
    }

    /// <summary>Undoes the table's last operation.</summary>
    public ChildTableResult Undo()
    {
        if (Gate() is { } refused)
        {
            return refused;
        }

        return Table.UndoRow()
            ? ChildTableResult.Ok()
            : ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "There is nothing to undo.");
    }

    /// <summary>Redoes the table's last undone operation.</summary>
    public ChildTableResult Redo()
    {
        if (Gate() is { } refused)
        {
            return refused;
        }

        return Table.RedoRow()
            ? ChildTableResult.Ok()
            : ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "There is nothing to redo.");
    }
}
