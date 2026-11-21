namespace Spotledger.Client.Metadata;

/// <summary>
/// The child table's operation-local row journal: what a table control's undo and redo act
/// on. The draft is the one mutation authority, so the journal lives here (never in the
/// control): every row edit, add, insert, delete and move records the inverse data it
/// needs — the previous value of an edited cell, the removed row itself (so its id,
/// values and dirty flags come back untouched), the index a row had before a move — and
/// <see cref="UndoRow"/>/<see cref="RedoRow"/> replay that data.
///
/// A journal is <b>per table</b>: an undo never touches the parent's fields or another
/// table's rows. It is bounded (<see cref="JournalDepth"/> operations, 100 by default) and
/// is cleared by <c>LoadRows</c>, <c>Revert</c> and <c>MarkSaved</c> — the moments the
/// original the journal undoes towards is replaced. A new edit after an undo drops the
/// redo tail, exactly as a text editor does.
///
/// A bulk edit (a paste, a fill-down) is <b>one</b> journal entry and <b>one</b>
/// notification: open it with <see cref="Begin"/>, make the row calls, then
/// <c>Commit()</c> (or dispose it).
/// </summary>
public sealed partial class DocumentDraft
{
    /// <summary>The mutable child-table state of one table field (see
    /// <see cref="DocumentDraft.Tables"/>).</summary>
    public sealed partial class TableDraft
    {
        /// <summary>The journal's default bound: 100 operations per table.</summary>
        public const int DefaultJournalDepth = 100;

        private readonly List<JournalEntry> _journal = new();
        private JournalEntry? _open;
        private int _cursor;
        private int _depth = DefaultJournalDepth;

        /// <summary>How many operations the journal keeps before dropping the oldest.
        /// The draft's constructor sets it (100 when it is not told otherwise).</summary>
        public int JournalDepth
        {
            get => _depth;
            set
            {
                if (value < 1)
                {
                    throw new ArgumentOutOfRangeException(nameof(value), value, "The journal keeps at least one operation.");
                }

                _depth = value;
                TrimJournal();
            }
        }

        /// <summary>True when there is an operation to undo.</summary>
        public bool CanUndo => _cursor > 0;

        /// <summary>True when there is an undone operation to redo.</summary>
        public bool CanRedo => _cursor < _journal.Count;

        /// <summary>How many operations the journal currently holds.</summary>
        public int JournalCount => _journal.Count;

        /// <summary>The description of the operation <see cref="UndoRow"/> would undo.</summary>
        public string? UndoDescription => CanUndo ? _journal[_cursor - 1].Description : null;

        /// <summary>The description of the operation <see cref="RedoRow"/> would redo.</summary>
        public string? RedoDescription => CanRedo ? _journal[_cursor].Description : null;

        /// <summary>True while a <see cref="Begin"/> operation has not been committed.</summary>
        public bool IsOperationOpen => _open is not null;

        /// <summary>
        /// Opens a bulk operation: every row change until <c>Commit()</c> (or the returned
        /// object's disposal) becomes one journal entry and raises one notification, never
        /// one per cell. Nested operations on the same table are refused.
        /// </summary>
        /// <param name="description">What the user did ("paste 200 rows"), kept for undo labels.</param>
        public RowOperation Begin(string description)
        {
            ArgumentNullException.ThrowIfNull(description);
            if (_open is not null)
            {
                throw new InvalidOperationException(
                    $"The table {Field.Name} already has an open operation; commit it before starting another.");
            }

            _open = new JournalEntry(description, isOperation: true);
            _quiet++;
            return new RowOperation(this, _open);
        }

        /// <summary>
        /// Undoes the last operation of this table: a cell edit gets its previous value back,
        /// an added row goes away, a deleted row returns with its id, values and position,
        /// a move returns to where it came from. Answers false when there is nothing to undo.
        /// </summary>
        public bool UndoRow()
        {
            EnsureNoOpenOperation("undo");
            if (_cursor == 0)
            {
                return false;
            }

            var entry = _journal[--_cursor];
            Apply(entry, inverse: true);
            Announce(entry, isUndo: true, isRedo: false);
            return true;
        }

        /// <summary>
        /// Redoes the last undone operation of this table, in the same shape
        /// <see cref="UndoRow"/> restores it. Answers false when there is nothing to redo.
        /// </summary>
        public bool RedoRow()
        {
            EnsureNoOpenOperation("redo");
            if (_cursor == _journal.Count)
            {
                return false;
            }

            var entry = _journal[_cursor++];
            Apply(entry, inverse: false);
            Announce(entry, isUndo: false, isRedo: true);
            return true;
        }

        private void EnsureNoOpenOperation(string what)
        {
            if (_open is not null)
            {
                throw new InvalidOperationException(
                    $"The table {Field.Name} has an open operation; commit it before you {what}.");
            }
        }

        /// <summary>Forgets every operation: the originals the journal undoes towards changed.</summary>
        internal void ClearJournal()
        {
            _journal.Clear();
            if (_open is not null)
            {
                // The basis changed under an open bulk operation: abandon it (its Commit
                // becomes a no-op) and release the notification hold it took.
                _open.Abandoned = true;
                _open = null;
                _quiet = Math.Max(0, _quiet - 1);
            }

            _cursor = 0;
        }

        /// <summary>Records one step of the caller's change (a single row call or a bulk one).</summary>
        private void Record(JournalStep step, string description)
        {
            if (_applying > 0)
            {
                // An undo, a redo, a load or a revert is not the user's edit.
                return;
            }

            if (_open is { } open)
            {
                open.Steps.Add(step);
                return;
            }

            var entry = new JournalEntry(description, isOperation: false);
            entry.Steps.Add(step);
            Push(entry);
        }

        private void Push(JournalEntry entry)
        {
            if (_cursor < _journal.Count)
            {
                // A new edit after an undo: the redone-future no longer exists.
                _journal.RemoveRange(_cursor, _journal.Count - _cursor);
            }

            _journal.Add(entry);
            _cursor = _journal.Count;
            TrimJournal();
        }

        private void TrimJournal()
        {
            var excess = _journal.Count - _depth;
            if (excess <= 0)
            {
                return;
            }

            _journal.RemoveRange(0, excess);
            _cursor = Math.Max(0, _cursor - excess);
        }

        private void Apply(JournalEntry entry, bool inverse)
        {
            _applying++;
            try
            {
                if (inverse)
                {
                    for (var i = entry.Steps.Count - 1; i >= 0; i--)
                    {
                        ApplyStep(entry.Steps[i], inverse: true);
                    }

                    return;
                }

                foreach (var step in entry.Steps)
                {
                    ApplyStep(step, inverse: false);
                }
            }
            finally
            {
                _applying--;
            }
        }

        private void ApplyStep(JournalStep step, bool inverse)
        {
            switch (step)
            {
                case CellStep cell:
                    AssignValue(cell.Row, cell.Field, inverse ? cell.Before : cell.After, inverse ? cell.HadField : true);
                    break;
                case InsertStep insert when inverse:
                    RemoveAtRaw(insert.Index);
                    break;
                case InsertStep insert:
                    InsertAtRaw(insert.Index, insert.Row);
                    break;
                case RemoveStep remove when inverse:
                    InsertAtRaw(remove.Index, remove.Row);
                    break;
                case RemoveStep remove:
                    RemoveAtRaw(remove.Index);
                    break;
                case MoveStep move:
                    MoveRaw(move.Row, inverse ? move.From : move.To);
                    break;
                default:
                    break;
            }
        }

        private void AssignValue(RowDraft row, string field, FieldValue? value, bool hadField)
        {
            if (!hadField)
            {
                // The cell did not exist before the edit: the undo takes it away again.
                row._values.Remove(field);
                return;
            }

            if (row._values.TryGetValue(field, out var state))
            {
                if (state.IsMasked)
                {
                    // A masked cell never takes a value back (and an undo must not throw midway).
                    return;
                }

                state.Value = value ?? FieldValue.Null;
            }
            else
            {
                row._values[field] = new FieldState(value ?? FieldValue.Null);
            }

            NotifyValueChanged(row, field);
        }

        private void InsertAtRaw(int index, RowDraft row)
        {
            if (_rows.Contains(row))
            {
                return;
            }

            _rows.Insert(Math.Clamp(index, 0, _rows.Count), row);
            Attach(row);
            RefreshPermissions();
        }

        private void RemoveAtRaw(int index)
        {
            if (index < 0 || index >= _rows.Count)
            {
                return;
            }

            _rows.RemoveAt(index);
            RefreshPermissions();
        }

        private void MoveRaw(RowDraft row, int to)
        {
            var from = _rows.IndexOf(row);
            if (from < 0)
            {
                return;
            }

            _rows.RemoveAt(from);
            _rows.Insert(Math.Clamp(to, 0, _rows.Count), row);
            RefreshPermissions();
        }

        /// <summary>Raises the notifications an undone or redone entry stands for.</summary>
        private void Announce(JournalEntry entry, bool isUndo, bool isRedo)
        {
            if (entry.Steps.Count == 0)
            {
                return;
            }

            var change = new StructureChangedEventArgs(
                entry.IsOperation ? StructureChangeKind.BulkOperation : entry.Steps[0].Kind,
                Field.Name,
                entry.Steps[0] is CellStep only && entry.Steps.Count == 1 ? only.Field : null,
                entry.Steps.Select(step => step.RowKey).ToArray(),
                entry.Steps.Min(step => step.Index),
                entry.Steps.Count,
                entry.Steps.OfType<MoveStep>().Select(move => (int?)move.From).FirstOrDefault(),
                entry.Description,
                isUndo,
                isRedo,
                entry.Steps.Max(step => step.Index));

            if (change.Kind == StructureChangeKind.CellEdited && change.Field is { } field)
            {
                // A single cell edit was undone: the tree gets the same field notification.
                FieldRaised?.Invoke(FieldPath.OfRow(Field.Name, change.Index + 1, field));
            }

            StructureRaised?.Invoke(change);
        }

        private void NotifyStructure(StructureChangedEventArgs change)
        {
            if (_quiet > 0)
            {
                return;
            }

            StructureRaised?.Invoke(change);
        }

        private void Commit(RowOperation operation)
        {
            if (operation.Entry.Abandoned)
            {
                return;
            }

            if (!ReferenceEquals(_open, operation.Entry))
            {
                throw new InvalidOperationException($"This operation does not belong to the table {Field.Name}.");
            }

            _open = null;
            _quiet--;
            if (operation.Entry.Steps.Count == 0)
            {
                return;
            }

            Push(operation.Entry);
            StructureRaised?.Invoke(new StructureChangedEventArgs(
                StructureChangeKind.BulkOperation,
                Field.Name,
                null,
                operation.Entry.Steps.Select(step => step.RowKey).ToArray(),
                operation.Entry.Steps.Min(step => step.Index),
                operation.Entry.Steps.Count,
                null,
                operation.Entry.Description,
                lastIndex: operation.Entry.Steps.Max(step => step.Index)));
        }

        /// <summary>One open bulk operation: collect the row changes, then commit them as one entry.</summary>
        public sealed class RowOperation : IDisposable
        {
            private TableDraft? _table;

            internal RowOperation(TableDraft table, JournalEntry entry)
            {
                _table = table;
                Entry = entry;
            }

            internal JournalEntry Entry { get; }

            /// <summary>True until the operation was committed.</summary>
            public bool IsOpen => _table is not null;

            /// <summary>Closes the operation: one journal entry and one notification.</summary>
            public void Commit()
            {
                var table = _table;
                _table = null;
                table?.Commit(this);
            }

            /// <summary>Disposal commits an operation nobody committed, so a bulk edit is
            /// never left silent and unjournaled.</summary>
            public void Dispose() => Commit();
        }

        internal abstract record JournalStep(StructureChangeKind Kind, int Index, string? RowKey);

        internal sealed record CellStep(RowDraft Row, string Field, FieldValue? Before, FieldValue? After, bool HadField, int Index)
            : JournalStep(StructureChangeKind.CellEdited, Index, Row.Id);

        internal sealed record InsertStep(RowDraft Row, int Index, StructureChangeKind Kind)
            : JournalStep(Kind, Index, Row.Id);

        internal sealed record RemoveStep(RowDraft Row, int Index)
            : JournalStep(StructureChangeKind.RowRemoved, Index, Row.Id);

        internal sealed record MoveStep(RowDraft Row, int From, int To)
            : JournalStep(StructureChangeKind.RowMoved, To, Row.Id);

        internal sealed class JournalEntry(string description, bool isOperation)
        {
            public string Description { get; } = description;

            public bool IsOperation { get; } = isOperation;

            public bool Abandoned { get; set; }

            public List<JournalStep> Steps { get; } = new();
        }
    }
}
