namespace Spotledger.Client.Designer;

/// <summary>One operator in the condition builder's picker, with its words for a person.</summary>
public sealed record ConditionOperatorChoice(ConditionOperator Operator)
{
    /// <summary>Every operator, in the order the picker shows them.</summary>
    public static IReadOnlyList<ConditionOperatorChoice> All { get; } =
        Enum.GetValues<ConditionOperator>().Select(op => new ConditionOperatorChoice(op)).ToList();

    /// <summary>The operator's words (<c>is at least</c>).</summary>
    public string Label => ConditionSyntax.Label(Operator);

    /// <summary>The choice for an operator.</summary>
    public static ConditionOperatorChoice Find(ConditionOperator op) =>
        All.First(choice => choice.Operator == op);

    /// <summary>The picker's words for the row.</summary>
    public override string ToString() => Label;
}

/// <summary>One editable row of the visual condition builder.</summary>
public sealed class ConditionRowDraft
{
    /// <summary>Creates a row.</summary>
    public ConditionRowDraft(string field, ConditionOperator op, string value)
    {
        Field = field;
        Operator = op;
        Value = value;
    }

    /// <summary>The field the row tests, by storage name.</summary>
    public string Field { get; set; }

    /// <summary>What the row tests.</summary>
    public ConditionOperator Operator { get; set; }

    /// <summary>The value as the user typed it, unquoted (<c>Low, Medium</c> for "is one of").</summary>
    public string Value { get; set; }

    /// <summary>The fields the row's picker offers (set by the inspector when it loads the rows).</summary>
    public IReadOnlyList<CanvasField> Fields { get; set; } = Array.Empty<CanvasField>();

    /// <summary>The operators the row's picker offers.</summary>
    public IReadOnlyList<ConditionOperatorChoice> Operators { get; } = ConditionOperatorChoice.All;

    /// <summary>The row's operator as a picker choice (the view's two-way binding).</summary>
    public ConditionOperatorChoice OperatorChoice
    {
        get => ConditionOperatorChoice.Find(Operator);
        set
        {
            if (value is not null)
            {
                Operator = value.Operator;
            }
        }
    }

    /// <summary>The row's field as a picker choice (the view's two-way binding).</summary>
    public CanvasField? FieldChoice
    {
        get => Fields.FirstOrDefault(f => string.Equals(f.Name, Field, StringComparison.Ordinal));
        set
        {
            if (value is not null)
            {
                Field = value.Name;
            }
        }
    }
}

/// <summary>
/// The visual condition builder's rows, as data. It never compiles anything: each change asks the
/// server to compile the rows into expression text, and the builder shows that text as a preview.
/// </summary>
public sealed class ConditionDraft
{
    private readonly List<ConditionRowDraft> _rows = new();

    /// <summary>How the rows combine.</summary>
    public ConditionJoin Join { get; set; } = ConditionJoin.And;

    /// <summary>The rows, in order.</summary>
    public IReadOnlyList<ConditionRowDraft> Rows => _rows;

    /// <summary>Whether the builder has no rows (so there is no condition at all).</summary>
    public bool IsEmpty => _rows.Count == 0;

    /// <summary>Adds a row and returns it.</summary>
    public ConditionRowDraft Add(string field, ConditionOperator op = ConditionOperator.Equals, string value = "")
    {
        var row = new ConditionRowDraft(field, op, value);
        _rows.Add(row);
        return row;
    }

    /// <summary>Removes the row at <paramref name="index"/>, when it exists.</summary>
    public bool RemoveAt(int index)
    {
        if (index < 0 || index >= _rows.Count)
        {
            return false;
        }

        _rows.RemoveAt(index);
        return true;
    }

    /// <summary>Replaces every row (loading a field's existing condition into the builder).</summary>
    public void Replace(IEnumerable<ConditionRowDraft> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        _rows.Clear();
        _rows.AddRange(rows);
    }

    /// <summary>Clears every row.</summary>
    public void Clear() => _rows.Clear();

    /// <summary>The rows as the server's condition shape; null when there are none.</summary>
    public ConditionExpression? ToExpression()
    {
        if (_rows.Count == 0)
        {
            return null;
        }

        return new ConditionExpression(Join, _rows.Select(ToRow).ToList());
    }

    /// <summary>One row as the server's condition row, with the literals typed for the operator.</summary>
    public static ConditionRow ToRow(ConditionRowDraft row)
    {
        ArgumentNullException.ThrowIfNull(row);
        var count = ConditionSyntax.ValueCount(row.Operator);
        IReadOnlyList<ConditionValue> values;
        if (count == 0)
        {
            values = Array.Empty<ConditionValue>();
        }
        else if (count is null)
        {
            values = row.Value
                .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
                .Select(ConditionValue.OfText)
                .ToList();
        }
        else
        {
            values = new[] { ConditionValue.OfText(row.Value.Trim()) };
        }

        return new ConditionRow(row.Field, row.Operator, values);
    }

    /// <summary>A person's words for the whole condition (the summary above the preview line).</summary>
    public string Describe() => _rows.Count == 0
        ? "No condition"
        : DesignerOpText.Describe(new ConditionExpression(Join, _rows.Select(ToRow).ToList()));
}
