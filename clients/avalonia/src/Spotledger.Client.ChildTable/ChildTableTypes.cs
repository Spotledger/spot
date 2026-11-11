using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.ChildTable;

/// <summary>What a column's totals-row cell shows. Sums and averages are exact decimal
/// arithmetic over the field's values; counts are of non-empty cells.</summary>
public enum ChildTableTotal
{
    /// <summary>The exact sum of the column's numeric values.</summary>
    Sum,

    /// <summary>How many rows hold a value in the column.</summary>
    Count,

    /// <summary>The exact sum divided by the count of rows holding a value, rounded half
    /// away from zero to the field's scale for display only (never stored or sent).</summary>
    Average,
}

/// <summary>One column of the table as the view definition declares it.</summary>
/// <param name="Field">The row field's storage name.</param>
/// <param name="Width">The column width; absent means a default by field kind.</param>
/// <param name="Total">The totals-row cell for the column; absent means none.</param>
public sealed record ChildTableColumn(string Field, GridColumnWidth? Width = null, ChildTableTotal? Total = null);

/// <summary>The table's view definition: which row fields are columns and which carry a
/// total. With no view the table shows every visible scalar field of the row model.</summary>
/// <param name="Columns">The columns in display order.</param>
public sealed record ChildTableView(IReadOnlyList<ChildTableColumn> Columns);

/// <summary>A column resolved against the row model.</summary>
/// <param name="Field">The row field's model.</param>
/// <param name="Spec">The view's column declaration.</param>
public sealed record ChildTableColumnInfo(FieldModel Field, ChildTableColumn Spec);

/// <summary>Why an operation, an edit or a cell was refused. <see cref="None"/> means allowed.</summary>
public enum ChildTableRefusal
{
    /// <summary>Allowed.</summary>
    None,

    /// <summary>The host has not granted the write right on the document.</summary>
    NoWriteRight,

    /// <summary>The document is posted.</summary>
    DocumentPosted,

    /// <summary>The document is cancelled.</summary>
    DocumentCancelled,

    /// <summary>The document carries a state this client does not know; it fails closed.</summary>
    DocumentStateUnknown,

    /// <summary>The table field is read-only (metadata, permission or rule).</summary>
    TableReadOnly,

    /// <summary>The table field is masked for this caller.</summary>
    TableMasked,

    /// <summary>The cell's field is read-only (metadata, permission or rule).</summary>
    CellReadOnly,

    /// <summary>The cell is masked for this caller.</summary>
    CellMasked,

    /// <summary>The cell's field is hidden by a rule.</summary>
    CellHidden,

    /// <summary>The cell's field kind is not edited inline (long text, JSON, secrets, files).</summary>
    CellKindNotEditable,

    /// <summary>The row or column index is outside the table.</summary>
    OutOfRange,

    /// <summary>Nothing to do (no rows selected, nothing to undo).</summary>
    NothingToDo,

    /// <summary>The input could not be applied; the message lists why.</summary>
    Invalid,
}

/// <summary>One pasted cell that could not be applied.</summary>
/// <param name="Row">The 1-based table row the cell lands on.</param>
/// <param name="Column">The column's field name, or null for a cell beyond the last column.</param>
/// <param name="Text">The pasted text.</param>
/// <param name="Message">Why it was refused.</param>
public sealed record PasteCellError(int Row, string? Column, string Text, string Message);

/// <summary>What a paste did. A paste is all or nothing: with any error nothing is applied.</summary>
/// <param name="Applied">True when the cells were written.</param>
/// <param name="RowsCreated">New rows appended.</param>
/// <param name="CellsWritten">Cells written.</param>
/// <param name="Errors">Every cell that could not be applied, in order.</param>
public sealed record PasteReport(bool Applied, int RowsCreated, int CellsWritten, IReadOnlyList<PasteCellError> Errors);

/// <summary>The outcome of one table operation: whether it ran, why not, and where the cursor
/// and selection should go next.</summary>
public sealed class ChildTableResult
{
    private ChildTableResult(ChildTableRefusal refusal, string? message)
    {
        Refusal = refusal;
        Message = message;
    }

    /// <summary>True when the operation ran.</summary>
    public bool Succeeded => Refusal == ChildTableRefusal.None;

    /// <summary>Why the operation did not run, or <see cref="ChildTableRefusal.None"/>.</summary>
    public ChildTableRefusal Refusal { get; }

    /// <summary>A short human-readable reason; null on success.</summary>
    public string? Message { get; }

    /// <summary>Where the cursor goes next (row, column), when the operation names one.</summary>
    public (int Row, int Column)? Cursor { get; private init; }

    /// <summary>The rows to select next, when the operation names them.</summary>
    public IReadOnlyList<int>? Selection { get; private init; }

    /// <summary>The paste report, for a paste.</summary>
    public PasteReport? Paste { get; private init; }

    /// <summary>An operation that ran, naming where the cursor and selection go next when it does.</summary>
    /// <param name="cursor">The cell to move the cursor to; null leaves it where it is.</param>
    /// <param name="selection">The rows to select; null leaves the selection as it is.</param>
    /// <param name="paste">The paste report, for a paste.</param>
    public static ChildTableResult Ok(
        (int Row, int Column)? cursor = null, IReadOnlyList<int>? selection = null, PasteReport? paste = null)
        => new(ChildTableRefusal.None, null) { Cursor = cursor, Selection = selection, Paste = paste };

    /// <summary>An operation that did not run, with the refusal and a sentence for the user.</summary>
    /// <param name="refusal">Why it did not run.</param>
    /// <param name="message">The sentence to show; null uses the refusal's standard one.</param>
    /// <param name="paste">The paste report, for a paste.</param>
    public static ChildTableResult Refused(ChildTableRefusal refusal, string? message = null, PasteReport? paste = null)
        => new(refusal, message ?? Describe(refusal)) { Paste = paste };

    /// <summary>The standard sentence for a refusal.</summary>
    public static string Describe(ChildTableRefusal refusal) => refusal switch
    {
        ChildTableRefusal.None => string.Empty,
        ChildTableRefusal.NoWriteRight => "You do not have the right to change this document.",
        ChildTableRefusal.DocumentPosted => "A posted document cannot be changed.",
        ChildTableRefusal.DocumentCancelled => "A cancelled document cannot be changed.",
        ChildTableRefusal.DocumentStateUnknown => "This document is in a state this client does not know, so it is read-only.",
        ChildTableRefusal.TableReadOnly => "This table is read-only.",
        ChildTableRefusal.TableMasked => "This table is hidden from you.",
        ChildTableRefusal.CellReadOnly => "This cell is read-only.",
        ChildTableRefusal.CellMasked => "This cell is masked.",
        ChildTableRefusal.CellHidden => "This cell is hidden.",
        ChildTableRefusal.CellKindNotEditable => "This kind of cell is not edited in the table.",
        ChildTableRefusal.OutOfRange => "That row or column is not in the table.",
        ChildTableRefusal.NothingToDo => "There is nothing to do.",
        _ => "The input could not be applied.",
    };
}

/// <summary>What a lookup source factory is told about the cell being edited, so the lookup
/// can send the row snapshot as <c>doc</c> and the document as <c>parent</c>.</summary>
/// <param name="Field">The link field being edited.</param>
/// <param name="Table">The table field's name.</param>
/// <param name="RowIndex">The 0-based row index.</param>
/// <param name="Row">A snapshot of the row's current values by field name.</param>
/// <param name="Draft">The parent document's draft.</param>
public sealed record ChildTableLookupRequest(
    FieldModel Field,
    string Table,
    int RowIndex,
    IReadOnlyDictionary<string, FieldValue> Row,
    DocumentDraft Draft);
