namespace Spotledger.Client.Metadata;

/// <summary>
/// One row of a child table, typed: its <see cref="Id"/> when the server knows it (null
/// for a row added locally), and the row's values by storage name.
/// </summary>
/// <param name="Id">The row's id, or null when the row was added locally.</param>
/// <param name="Values">The field values by storage name (scalar fields only).</param>
public sealed record TableRowValue(
    string? Id,
    IReadOnlyDictionary<string, FieldValue> Values)
{
    /// <summary>Creates a row wrapper over a snapshot of a dictionary.</summary>
    public static TableRowValue Existing(string id, IReadOnlyDictionary<string, FieldValue> values) => new(id, values);

    /// <summary>Creates a new (not yet saved) row.</summary>
    public static TableRowValue New(IReadOnlyDictionary<string, FieldValue> values) => new(null, values);
}

/// <summary>
/// A child table value: the rows in display order with the table's row model.
/// </summary>
/// <param name="Rows">The rows, in order.</param>
/// <param name="RowModel">The row's DocType model (null for an untyped table).</param>
public sealed record TableFieldValue : FieldValue
{
    /// <summary>Creates the table value.</summary>
    public TableFieldValue(IReadOnlyList<TableRowValue> Rows, DocTypeModel? RowModel)
    {
        this.Rows = Rows;
        this.RowModel = RowModel;
    }

    /// <summary>The rows, in order.</summary>
    public IReadOnlyList<TableRowValue> Rows { get; init; }

    /// <summary>The row's DocType model (null for an untyped table).</summary>
    public DocTypeModel? RowModel { get; init; }

    /// <summary>True when the table carries no rows.</summary>
    public bool IsEmpty => Rows.Count == 0;
}