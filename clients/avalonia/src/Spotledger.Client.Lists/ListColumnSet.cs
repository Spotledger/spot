using Spotledger.Client.Grid;

namespace Spotledger.Client.Lists;

/// <summary>The columns of one list, in order: what the query asks for, what the grid shows and how a
/// record becomes a grid row. The row key is the document id (opaque to the grid).</summary>
public sealed class ListColumnSet
{
    private readonly ListCellFormatter _formatter;

    /// <summary>Creates the set.</summary>
    public ListColumnSet(IReadOnlyList<ListColumn> columns, ListCellFormatter? formatter = null)
    {
        Columns = columns ?? throw new ArgumentNullException(nameof(columns));
        _formatter = formatter ?? new ListCellFormatter();
        FieldNames = columns.Select(c => c.Field.Name).ToArray();
    }

    /// <summary>The columns.</summary>
    public IReadOnlyList<ListColumn> Columns { get; }

    /// <summary>The field names the query asks for.</summary>
    public IReadOnlyList<string> FieldNames { get; }

    /// <summary>The grid column models, index-aligned with <see cref="Columns"/>.</summary>
    public IReadOnlyList<GridColumnModel> GridColumns => Columns.Select(c => c.Model).ToArray();

    /// <summary>Builds the grid row of a record.</summary>
    public GridRow ToRow(ListRecord record, IReadOnlyDictionary<(string, string), string> labels)
    {
        var cells = new GridCell[Columns.Count];
        for (var i = 0; i < cells.Length; i++)
        {
            var column = Columns[i];
            record.Values.TryGetValue(column.Field.Name, out var value);
            var perColumn = column.Field.LinkTo is { } target
                ? new LabelView(labels, target)
                : null;
            cells[i] = _formatter.Format(column, record.Values.ContainsKey(column.Field.Name) ? value : null, perColumn);
        }

        return new GridRow(record.Id, cells);
    }

    private sealed class LabelView(IReadOnlyDictionary<(string, string), string> labels, string target) : IReadOnlyDictionary<string, string>
    {
        public string this[string key] => labels[(target, key)];
        public IEnumerable<string> Keys => labels.Where(p => p.Key.Item1 == target).Select(p => p.Key.Item2);
        public IEnumerable<string> Values => labels.Where(p => p.Key.Item1 == target).Select(p => p.Value);
        public int Count => labels.Count;
        public bool ContainsKey(string key) => labels.ContainsKey((target, key));
        public bool TryGetValue(string key, out string value)
        {
            if (labels.TryGetValue((target, key), out var found))
            {
                value = found;
                return true;
            }

            value = string.Empty;
            return false;
        }

        public IEnumerator<KeyValuePair<string, string>> GetEnumerator()
            => labels.Where(p => p.Key.Item1 == target).Select(p => KeyValuePair.Create(p.Key.Item2, p.Value)).GetEnumerator();
        System.Collections.IEnumerator System.Collections.IEnumerable.GetEnumerator() => GetEnumerator();
    }
}
