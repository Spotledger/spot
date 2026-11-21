namespace Spotledger.Client.Metadata;

/// <summary>
/// What a <see cref="DocumentDraft.StructureChanged"/> notification reports. The row kinds
/// describe one call: <see cref="RowAdded"/> for <c>AddRow</c>, <see cref="RowInserted"/>
/// for <c>AddRowAt</c>, <see cref="RowRemoved"/> for <c>RemoveRowAt</c>,
/// <see cref="RowMoved"/> for <c>MoveRow</c> and <see cref="CellEdited"/> for a single
/// <c>RowDraft.SetValue</c>. <see cref="BulkOperation"/> stands for every change of one
/// <c>Begin</c>/<c>Commit</c> operation (a paste, a fill-down): the draft never reports the
/// cells of a bulk call one by one.
/// </summary>
public enum StructureChangeKind
{
    /// <summary>The whole document was loaded: every table was replaced.</summary>
    DocumentLoaded,

    /// <summary>A table was loaded (<c>LoadRows</c>): its rows and ids were replaced.</summary>
    RowsLoaded,

    /// <summary>The draft was reverted to its loaded values (one batched notification).</summary>
    Reverted,

    /// <summary>The current values became the saved originals (one batched notification).</summary>
    Saved,

    /// <summary>A permission snapshot was applied; rows may have been re-masked or re-keyed.</summary>
    PermissionsApplied,

    /// <summary>A row was appended (<c>AddRow</c>).</summary>
    RowAdded,

    /// <summary>A row was inserted at a position (<c>AddRowAt</c>).</summary>
    RowInserted,

    /// <summary>A row was removed (<c>RemoveRowAt</c>).</summary>
    RowRemoved,

    /// <summary>A row changed position (<c>MoveRow</c>).</summary>
    RowMoved,

    /// <summary>One row cell changed (<c>RowDraft.SetValue</c>).</summary>
    CellEdited,

    /// <summary>Every change of one journaled operation (a paste, a fill-down).</summary>
    BulkOperation,
}

/// <summary>
/// What changed in the draft's child rows, carried by
/// <see cref="DocumentDraft.StructureChanged"/>: the kind, the table, the row keys the
/// change touched, and the position range it touched. Positions are <b>0-based</b>
/// indexes into <see cref="DocumentDraft.TableDraft.Rows"/> — the same numbering
/// <c>RemoveRowAt</c>/<c>MoveRow</c> take; the 1-based numbering belongs to field paths
/// (which <see cref="DocumentDraft.FieldChanged"/> carries). A row key is the row's
/// server id (<c>null</c> for a row the server has not named yet), so a notification
/// survives reordering; errors and journal entries are keyed the same way.
/// </summary>
public sealed class StructureChangedEventArgs : EventArgs
{
    /// <summary>Creates a notification.</summary>
    /// <param name="kind">What changed.</param>
    /// <param name="table">The table field's name; null for a document-wide change.</param>
    /// <param name="field">The cell's field name, for <see cref="StructureChangeKind.CellEdited"/>.</param>
    /// <param name="rowKeys">The server ids of the rows touched (null entries for new rows).</param>
    /// <param name="index">The 0-based position the change starts at; -1 when it names none.</param>
    /// <param name="count">How many rows the change touches.</param>
    /// <param name="fromIndex">The 0-based position a moved row came from.</param>
    /// <param name="description">The journal operation's description, when it has one.</param>
    /// <param name="isUndo">True when an undo produced this change.</param>
    /// <param name="isRedo">True when a redo produced this change.</param>
    /// <param name="lastIndex">The 0-based last position the change touched; when absent it
    /// is derived from <paramref name="index"/> and <paramref name="count"/>.</param>
    public StructureChangedEventArgs(
        StructureChangeKind kind,
        string? table = null,
        string? field = null,
        IReadOnlyList<string?>? rowKeys = null,
        int index = -1,
        int count = 0,
        int? fromIndex = null,
        string? description = null,
        bool isUndo = false,
        bool isRedo = false,
        int? lastIndex = null)
    {
        Kind = kind;
        Table = table;
        Field = field;
        RowKeys = rowKeys ?? Array.Empty<string?>();
        Index = index;
        Count = count;
        FromIndex = fromIndex;
        Description = description;
        IsUndo = isUndo;
        IsRedo = isRedo;
        LastIndex = lastIndex ?? (index < 0 ? -1 : index + Math.Max(count, 1) - 1);
    }

    /// <summary>What changed.</summary>
    public StructureChangeKind Kind { get; }

    /// <summary>The table field's name, or null for a document-wide change.</summary>
    public string? Table { get; }

    /// <summary>The cell's field name for a cell edit, otherwise null.</summary>
    public string? Field { get; }

    /// <summary>The server ids of the rows the change touched (null entries for new rows).</summary>
    public IReadOnlyList<string?> RowKeys { get; }

    /// <summary>The 0-based first position the change touched; -1 when it names none.</summary>
    public int Index { get; }

    /// <summary>How many row changes the notification stands for: one per row call, and
    /// the number of steps for a bulk operation.</summary>
    public int Count { get; }

    /// <summary>The 0-based position a moved row came from, when the change is a move.</summary>
    public int? FromIndex { get; }

    /// <summary>The journal operation's description (a bulk operation's label).</summary>
    public string? Description { get; }

    /// <summary>True when an undo produced this change.</summary>
    public bool IsUndo { get; }

    /// <summary>True when a redo produced this change.</summary>
    public bool IsRedo { get; }

    /// <summary>The 0-based last position the change touched (inclusive), or -1.</summary>
    public int LastIndex { get; }
}

/// <summary>
/// A row's validation state changed: the server attached (or the draft cleared) errors on
/// a row or on one of its cells. Carried by
/// <see cref="DocumentDraft.ValidationChanged"/>. Errors are keyed by the row's server id,
/// never by position — a reorder or a delete above the row re-keys nothing.
/// </summary>
public sealed class RowValidationChangedEventArgs : EventArgs
{
    /// <summary>Creates the notification.</summary>
    /// <param name="table">The table field's name.</param>
    /// <param name="rowKey">The row's server id, null for a row the server has not named.</param>
    /// <param name="field">The cell's field name; null when the errors are the row's own.</param>
    /// <param name="errors">The errors now attached (empty when they were cleared).</param>
    /// <param name="index">The row's 0-based position at the time of the change.</param>
    public RowValidationChangedEventArgs(
        string table,
        string? rowKey,
        string? field,
        IReadOnlyList<FieldError> errors,
        int index)
    {
        Table = table;
        RowKey = rowKey;
        Field = field;
        Errors = errors;
        Index = index;
    }

    /// <summary>The table field's name.</summary>
    public string Table { get; }

    /// <summary>The row's server id (null for a row the server has not named).</summary>
    public string? RowKey { get; }

    /// <summary>The cell's field name, or null when the errors are the row's own.</summary>
    public string? Field { get; }

    /// <summary>The errors now attached; empty when they were cleared.</summary>
    public IReadOnlyList<FieldError> Errors { get; }

    /// <summary>The row's 0-based position at the time of the change.</summary>
    public int Index { get; }
}
