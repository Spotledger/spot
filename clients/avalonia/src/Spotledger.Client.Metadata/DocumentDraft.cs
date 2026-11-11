using System.Text.Json;

namespace Spotledger.Client.Metadata;

/// <summary>
/// The typed value state of one document (the brief's <c>DocumentDraft</c>):
/// per-field <see cref="FieldState"/>, child tables with row add/remove/move (kept row
/// ids, new rows marked), dirty tracking, revert, and the merge-patch body the contract's
/// <c>PATCH</c> needs. The draft knows about document state, not pixels; change
/// notifications are plain .NET events (a renderer wraps them itself).
/// </summary>
public sealed partial class DocumentDraft
{
    /// <summary>Creates a draft over a DocType model. New drafts start from each
    /// field's metadata default (or the kind's empty value without one), then from every
    /// field's <c>default_expr</c> evaluated once in dependency order, and the rule
    /// engine evaluates the DocType's rules.</summary>
    /// <param name="model">The DocType's model.</param>
    /// <param name="documentId">The document's id, when known (a draft being edited).</param>
    /// <param name="version">The document's version the edits are based on.</param>
    /// <param name="timeProvider">The clock the expressions' <c>now()</c> reads; the
    /// system clock when absent.</param>
    /// <param name="journalDepth">How many row operations each table's undo journal keeps
    /// (100 when absent): the bound on the draft's operation-local row undo.</param>
    public DocumentDraft(
        DocTypeModel model,
        string? documentId = null,
        int version = 0,
        TimeProvider? timeProvider = null,
        int journalDepth = TableDraft.DefaultJournalDepth)
    {
        if (journalDepth < 1)
        {
            throw new ArgumentOutOfRangeException(nameof(journalDepth), journalDepth, "The row journal keeps at least one operation.");
        }

        Model = model;
        DocumentId = documentId;
        Version = version;
        _timeProvider = timeProvider;
        foreach (var field in model.Fields)
        {
            if (field.Kind == FieldKind.Table)
            {
                var rowModel = model.ChildTable(field.Name);
                var original = new TableFieldValue(Array.Empty<TableRowValue>(), rowModel);
                _fields[field.Name] = new FieldState(original, field);
                var table = new TableDraft(field, rowModel) { JournalDepth = journalDepth };
                table.ConfigureRow = (row, ordinal) => ConfigureRow(table, row, ordinal);
                table.ValueChanged = (row, name) => Rules.RefreshAfterRow(table, row, name);
                table.RowAdded = row => Rules.OnRowAdded(table, row);
                table.StructureRaised = OnStructureChanged;
                table.FieldRaised = OnFieldChanged;
                table.ValidationRaised = change => ValidationChanged?.Invoke(this, change);
                _tables[field.Name] = table;
                ApplyFieldPermissions(model.Name, field, _fields[field.Name]);
                continue;
            }

            var value = field.Default ?? (field.Multiple ? new MultiSelectFieldValue(Array.Empty<string>()) : field.Kind.EmptyValue());
            _fields[field.Name] = new FieldState(value, field);
            ApplyFieldPermissions(model.Name, field, _fields[field.Name]);
        }

        Rules.ApplyCreationDefaults();
        Rules.EvaluateAll();
    }

    /// <summary>The DocType model the draft's fields and tables follow.</summary>
    public DocTypeModel Model { get; }

    /// <summary>The document's id, when known.</summary>
    public string? DocumentId { get; private set; }

    /// <summary>The document's version the current edits are based on (for If-Match).</summary>
    public int Version { get; private set; }

    /// <summary>The field states by storage name (scalar and table fields alike).</summary>
    public IReadOnlyDictionary<string, FieldState> Fields => _fields;

    private readonly Dictionary<string, FieldState> _fields = new(StringComparer.Ordinal);

    /// <summary>The child-table drafts by table field name.</summary>
    public IReadOnlyDictionary<string, TableDraft> Tables => _tables;

    private readonly Dictionary<string, TableDraft> _tables = new(StringComparer.Ordinal);

    /// <summary>Raised after any value change (a field set, a row change): the argument
    /// is the changed field's path, <c>items[3].qty</c> for a row's field (rows from 1).</summary>
    public event EventHandler<string>? FieldChanged;

    /// <summary>
    /// Raised once for every structural change of the draft — a row added, inserted,
    /// removed or moved, a single cell edited, a bulk operation committed, and the
    /// document-wide load, revert, save-rebase and permission refresh. The payload says
    /// what changed (<see cref="StructureChangedEventArgs"/>). A bulk operation raises one
    /// notification for the whole call, never one per cell.
    /// </summary>
    public event EventHandler<StructureChangedEventArgs>? StructureChanged;

    /// <summary>
    /// Raised when a row's validation state changes: server errors attached to a row (the
    /// contract's <c>table[idx]</c> path) or to one of its cells (the <c>table[idx].field</c>
    /// path), and when an edit or a clear removes them. The payload names the table and the
    /// row's key, never a position.
    /// </summary>
    public event EventHandler<RowValidationChangedEventArgs>? ValidationChanged;

    /// <summary>&gt;0 while the draft raises one batched notification instead of the
    /// per-call ones (a load, a revert, a save-rebase, a permission refresh).</summary>
    private int _suppressEvents;

    private void OnStructureChanged(StructureChangedEventArgs change)
    {
        if (_suppressEvents > 0)
        {
            return;
        }

        StructureChanged?.Invoke(this, change);
    }

    /// <summary>Gets the current value of a field (null when the DocType has no such field).</summary>
    public FieldValue? Get(string field) => _fields.TryGetValue(field, out var state) ? state.Value : null;

    /// <summary>
    /// Sets the value of a field. A set takes <see cref="FieldValue"/> typed values;
    /// typed text goes through <see cref="FieldModel.TryParse"/> first. Raises
    /// <see cref="FieldChanged"/> even when the value did not change (the user's edit
    /// round trip may keep the same value; renderers decide).
    /// </summary>
    public void Set(string field, FieldValue value)
    {
        var state = RequireField(field);
        state.Value = value ?? FieldValue.Null;
        OnFieldChanged(field);
        Rules.RefreshAfter(field);
    }

    /// <summary>Sets a field from typed text through the field's parser; answers false
    /// and attaches the parse error when the text is not a value.</summary>
    public bool TrySetText(string field, string? text, NumberParsingSettings? settings = null)
    {
        var fieldModel = RequireFieldModel(field);
        if (!fieldModel.TryParse(text, out var value, out var error, settings))
        {
            RequireField(field).SetErrors(new[] { error });
            OnFieldChanged(field);
            return false;
        }

        RequireField(field).ClearErrors();
        Set(field, value!);
        return true;
    }

    /// <summary>True when any field or table row differs from its original.</summary>
    public bool IsDirty
    {
        get
        {
            foreach (var state in _fields.Values)
            {
                if (state.IsDirty)
                {
                    return true;
                }
            }

            foreach (var table in _tables.Values)
            {
                if (table.IsDirty)
                {
                    return true;
                }
            }

            return false;
        }
    }

    /// <summary>The paths of the changed fields and rows, in field order.</summary>
    public IReadOnlyList<string> DirtyPaths()
    {
        var paths = new List<string>();
        foreach (var field in Model.Fields)
        {
            if (_fields[field.Name].IsDirty)
            {
                paths.Add(FieldPath.OfField(field.Name));
            }

            if (_tables.TryGetValue(field.Name, out var table))
            {
                paths.AddRange(table.DirtyPaths());
            }
        }

        return paths;
    }

    /// <summary>Reverts every field and table to the values the document was loaded
    /// with (or the fresh defaults): editing starts over, the id and version stay. The
    /// row journals are cleared (their originals changed) and one batched
    /// <see cref="StructureChanged"/> reports the revert.</summary>
    public void Revert()
    {
        _suppressEvents++;
        try
        {
            foreach (var field in Model.Fields)
            {
                var state = _fields[field.Name];
                state.ReplaceLoadedValue(state.Original);
                state.ClearErrors();
                if (_tables.TryGetValue(field.Name, out var table))
                {
                    table.Revert();
                }
            }

            Rules.EvaluateAll();
        }
        finally
        {
            _suppressEvents--;
        }

        OnStructureChanged(new StructureChangedEventArgs(StructureChangeKind.Reverted));
    }

    /// <summary>Marks every current value as the saved original (after the server
    /// accepted the patch). The row journals are cleared and one batched
    /// <see cref="StructureChanged"/> reports the rebase.</summary>
    public void MarkSaved()
    {
        _suppressEvents++;
        try
        {
            foreach (var state in _fields.Values)
            {
                state.MarkSaved();
                state.ClearRevealed();
            }

            foreach (var table in _tables.Values)
            {
                table.MarkSaved();
            }
        }
        finally
        {
            _suppressEvents--;
        }

        OnStructureChanged(new StructureChangedEventArgs(StructureChangeKind.Saved));
    }

    /// <summary>
    /// The contract's merge-patch body for this draft's edits: only changed fields;
    /// a child table that changed is replaced in full, with the ids on kept rows and
    /// ids absent on new rows; an unchanged draft produces an empty body.
    /// </summary>
    public string ToPatch()
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            foreach (var field in Model.Fields)
            {
                var state = _fields[field.Name];
                if (state.IsMasked) { continue; }
                if (field.Kind == FieldKind.Table)
                {
                    if (_tables.TryGetValue(field.Name, out var table) && table.IsDirty)
                    {
                        table.WritePatch(writer, field.Name);
                    }

                    continue;
                }

                if (!state.IsMasked && state.IsDirty)
                {
                    ValueCodec.WriteScalar(writer, field.Name, state.Value);
                }
            }

            writer.WriteEndObject();
        }

        return System.Text.Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>
    /// Loads a document's JSON (<c>resource_get</c>'s <c>data</c> body): the id and
    /// version are kept, every field's value decoded by its kind, child tables decoded
    /// with row ids. Everything becomes the original; the draft is clean afterwards.
    /// </summary>
    public void LoadFrom(string documentJson)
    {
        using var document = JsonDocument.Parse(documentJson, new JsonDocumentOptions
        {
            AllowTrailingCommas = false,
            CommentHandling = JsonCommentHandling.Disallow,
        });
        LoadFromJson(document.RootElement);
    }

    /// <summary>Loads from a parsed document element (shared by <see cref="LoadFrom"/>).</summary>
    public void LoadFromJson(JsonElement element)
    {
        if (element.ValueKind != JsonValueKind.Object)
        {
            throw new ArgumentException("The document body is not a JSON object.", nameof(element));
        }

        _suppressEvents++;
        try
        {
            LoadFields(element);
        }
        finally
        {
            _suppressEvents--;
        }

        OnStructureChanged(new StructureChangedEventArgs(StructureChangeKind.DocumentLoaded));
    }

    /// <summary>
    /// The document's own state, decoded from the loaded body's <c>_state</c>
    /// (<see cref="DocumentStatus.Unknown"/> when the body carries none). Read-only here:
    /// it is not a right, and the draft never refuses a mutation because of it.
    /// </summary>
    public DocumentStatus Status { get; private set; } = DocumentStatus.Unknown;

    /// <summary>The <c>_state</c> text exactly as the server sent it (null when absent);
    /// kept so a value this client does not know is never lost.</summary>
    public string? RawStatus { get; private set; }

    private void LoadFields(JsonElement element)
    {
        Status = DocumentStatus.Unknown;
        RawStatus = null;
        foreach (var state in _fields.Values)
        {
            state.ClearRevealed();
            if (state.IsMasked) { state.LoadMasked(null); }
        }
        foreach (var table in _tables.Values)
        {
            foreach (var row in table.Rows)
            {
                foreach (var state in row.Values.Values) { state.ClearRevealed(); }
            }
        }
        foreach (var property in element.EnumerateObject())
        {
            if (property.Name == "id")
            {
                DocumentId = property.Value.ValueKind == JsonValueKind.String ? property.Value.GetString() : DocumentId;
                continue;
            }

            if (property.Name == "version")
            {
                if (property.Value.ValueKind == JsonValueKind.Number && property.Value.TryGetInt32(out var version))
                {
                    Version = version;
                }

                continue;
            }

            if (property.Name == "_state")
            {
                RawStatus = property.Value.ValueKind == JsonValueKind.String
                    ? property.Value.GetString()
                    : property.Value.ValueKind == JsonValueKind.Null ? null : property.Value.GetRawText();
                Status = DocumentStatusText.Decode(RawStatus);
                continue;
            }

            if (property.Name.StartsWith('_'))
            {
                // Server metadata (_state and friends) is read-only and not a field.
                continue;
            }

            var field = Model.Field(property.Name);
            if (field is null)
            {
                // The document carries a field the client's (older) metadata does not
                // know: keep it as raw JSON data so a later patch never drops it.
                if (!IsRestrictedUnknown(property.Name)) { UnknownProperties[property.Name] = property.Value.GetRawText(); }
                continue;
            }

            if (_fields[field.Name].IsMasked)
            {
                _fields[field.Name].LoadMasked(property.Value.ValueKind == JsonValueKind.String ? property.Value.GetString() : null);
                if (_tables.TryGetValue(field.Name, out var maskedTable))
                {
                    maskedTable.LoadRows(new TableFieldValue(Array.Empty<TableRowValue>(), maskedTable.RowModel));
                }
                continue;
            }
            if (field.Kind == FieldKind.Table)
            {
                var rowModel = Model.ChildTable(field.Name);
                var table = _tables[field.Name];
                var decoded = property.Value.ValueKind == JsonValueKind.Array
                    ? ValueCodec.DecodeTable(property.Value, rowModel, (ordinal, name) => IsRowMasked(table, ordinal, name))
                    : new TableFieldValue(Array.Empty<TableRowValue>(), rowModel);
                table.LoadRows(decoded);
                if (property.Value.ValueKind == JsonValueKind.Array)
                {
                    var index = 0;
                    foreach (var row in property.Value.EnumerateArray())
                    {
                        if (row.ValueKind != JsonValueKind.Object) { continue; }
                        foreach (var cell in row.EnumerateObject())
                        {
                            if (table.Rows[index].Values.TryGetValue(cell.Name, out var cellState) && cellState.IsMasked)
                            {
                                cellState.LoadMasked(cell.Value.ValueKind == JsonValueKind.String ? cell.Value.GetString() : null);
                            }
                        }
                        index++;
                    }
                    table.MarkSaved();
                }
                var tableState = _fields[field.Name];
                tableState.ReplaceLoadedValue(table.CurrentValue());
                // The loaded content is the original: a just-loaded draft is clean (the
                // brief's LoadFrom contract) even where a default preceded the load.
                tableState.MarkSaved();
                continue;
            }

            var value = ValueCodec.DecodeElement(property.Value, field);
            SetLoaded(field.Name, value);
        }

        LoadCurrency(element);
        Rules.EvaluateAll();
    }

    /// <summary>The raw JSON of fields the client's metadata does not know (kept so a
    /// later patch round trip never drops server data the client predates).</summary>
    public IDictionary<string, string> UnknownProperties { get; } = new Dictionary<string, string>(StringComparer.Ordinal);

    /// <summary>Sets a loaded value as the field's current value (without a change event:
    /// the load path fires one StructureChanged at the end).</summary>
    private void SetLoaded(string field, FieldValue value)
    {
        var state = RequireField(field);
        state.Value = value;
        // The loaded value is the original ("everything becomes the original; the draft
        // is clean afterwards"): without this, fields whose preceding default was the
        // kind's zero shape (datetime, select, json) stayed dirty after every load.
        state.MarkSaved();
    }

    /// <summary>The table draft for a table field.</summary>
    public TableDraft Table(string field)
    {
        if (!_tables.TryGetValue(field, out var table))
        {
            throw new KeyNotFoundException($"The DocType has no child table {field}.");
        }

        return table;
    }

    private FieldState RequireField(string field)
    {
        if (!_fields.TryGetValue(field, out var state))
        {
            throw new KeyNotFoundException($"The DocType {Model.Name} has no field {field}.");
        }

        return state;
    }

    private FieldModel RequireFieldModel(string field) =>
        Model.Field(field) ?? throw new KeyNotFoundException($"The DocType {Model.Name} has no field {field}.");

    private void OnFieldChanged(string field) => FieldChanged?.Invoke(this, field);

    /// <summary>The mutable child-table state of one table field (see
    /// <see cref="DocumentDraft.Tables"/>). The row journal half of this class lives in
    /// <c>DocumentDraft.TableDraft.Journal.cs</c>.</summary>
    public sealed partial class TableDraft
    {
        /// <summary>Creates the table draft (per table field, by the DocType draft).</summary>
        public TableDraft(FieldModel field, DocTypeModel? rowModel)
        {
            Field = field;
            RowModel = rowModel;
        }

        /// <summary>The table field's model.</summary>
        public FieldModel Field { get; }

        /// <summary>The row DocType's model, when known.</summary>
        public DocTypeModel? RowModel { get; }

        /// <summary>The rows, in display order.</summary>
        public IReadOnlyList<RowDraft> Rows => _rows;

        private readonly List<RowDraft> _rows = new();

        internal Action<RowDraft, int>? ConfigureRow { get; set; }

        /// <summary>Raised for a row value edit the draft must react to (the rule engine
        /// re-evaluates the row's affected rules).</summary>
        internal Action<RowDraft, string>? ValueChanged { get; set; }

        /// <summary>Raised once for a row the user just added: its default expressions
        /// apply and its rules run for the first time.</summary>
        internal Action<RowDraft>? RowAdded { get; set; }

        /// <summary>Reports what changed to the draft (which owns the public event).</summary>
        internal Action<StructureChangedEventArgs>? StructureRaised { get; set; }

        /// <summary>Reports a cell edit's field path to the draft.</summary>
        internal Action<string>? FieldRaised { get; set; }

        /// <summary>Reports a row's validation change to the draft.</summary>
        internal Action<RowValidationChangedEventArgs>? ValidationRaised { get; set; }

        /// <summary>&gt;0 while this table's per-call notifications are batched: a load,
        /// a revert, a save-rebase, an undo/redo application, an open bulk operation.</summary>
        private int _quiet;

        /// <summary>&gt;0 while the table applies a change that is not the user's edit
        /// (a load, a revert, a save-rebase, an undo/redo): no journal entries.</summary>
        private int _applying;

        /// <summary>Reports a row value edit to the rule engine (which re-evaluates the
        /// row's affected rules).</summary>
        internal void NotifyValueChanged(RowDraft row, string field)
        {
            // A row that is still being built (a load) is not in the table yet: its
            // rules run once, when the load finishes.
            if (ValueChanged is not null && _rows.Contains(row))
            {
                ValueChanged(row, field);
            }
        }

        /// <summary>
        /// The one path a row's cell edit takes: journal the inverse, clear the cell's
        /// server error, notify the field and the structure, then let the rules re-run.
        /// </summary>
        internal void OnRowValueSet(RowDraft row, string field, FieldValue? before, FieldValue after, bool hadField)
        {
            var index = _rows.IndexOf(row);
            if (index < 0)
            {
                // A row being built by a load: no journal, no event, no error to clear.
                return;
            }

            if (_applying == 0)
            {
                Record(new CellStep(row, field, before, after, hadField, index), $"edit {field}");
                if (row.Values.TryGetValue(field, out var state) && state.Errors.Count > 0)
                {
                    // A cell edit drops the cell's server error: the next save re-validates.
                    state.ClearErrors();
                    NotifyRowValidation(row, field);
                }
            }

            if (_quiet == 0)
            {
                FieldRaised?.Invoke(FieldPath.OfRow(Field.Name, index + 1, field));
                StructureRaised?.Invoke(new StructureChangedEventArgs(
                    StructureChangeKind.CellEdited, Field.Name, field, new[] { row.Id }, index, 1));
            }

            NotifyValueChanged(row, field);
        }

        /// <summary>Reports a row's (or a row cell's) validation change to the draft.</summary>
        internal void NotifyRowValidation(RowDraft row, string? field)
        {
            if (_quiet > 0)
            {
                return;
            }

            var errors = field is null
                ? row.RowErrors
                : row.Values.TryGetValue(field, out var state) ? (IReadOnlyList<FieldError>)state.Errors : Array.Empty<FieldError>();
            ValidationRaised?.Invoke(new RowValidationChangedEventArgs(Field.Name, row.Id, field, errors, _rows.IndexOf(row)));
        }

        internal void RefreshPermissions()
        {
            foreach (var row in _rows) { ConfigureRow?.Invoke(row, _rows.IndexOf(row) + 1); }
            var ordinal = 0;
            foreach (var snapshot in _originalRows)
            {
                var row = new RowDraft(snapshot.Id);
                foreach (var pair in snapshot.Values) { row._values[pair.Key] = pair.Value; }
                ConfigureRow?.Invoke(row, ++ordinal);
            }
        }

        internal TableFieldValue CurrentValue() => new(_rows.Select(row => new TableRowValue(row.Id,
            row.Values.ToDictionary(pair => pair.Key, pair => pair.Value.Value, StringComparer.Ordinal))).ToArray(), RowModel);

        /// <summary>The original rows the table was loaded with.</summary>
        private IReadOnlyList<RowSnapshot> _originalRows = Array.Empty<RowSnapshot>();

        /// <summary>True when the row set differs from the loaded one.</summary>
        public bool IsDirty => IsDirtySequence(_rows, _originalRows);

        private static bool IsDirtySequence(List<RowDraft> rows, IReadOnlyList<RowSnapshot> originals)
        {
            if (rows.Count != originals.Count)
            {
                return true;
            }

            for (var i = 0; i < rows.Count; i++)
            {
                var row = rows[i];
                var original = originals[i];
                if (!string.Equals(row.Id, original.Id, StringComparison.Ordinal))
                {
                    return true;
                }

                foreach (var pair in row.Values)
                {
                    if (!original.Values.TryGetValue(pair.Key, out var originalValue)
                        || !FieldState.ValuesEqual(originalValue.Original, pair.Value.Value))
                    {
                        return true;
                    }
                }

                if (original.Values.Count != row.Values.Count)
                {
                    return true;
                }
            }

            return false;
        }

        /// <summary>The paths of the changed rows: <c>items[3].qty</c> for a changed
        /// value, <c>items[3]</c> for a structurally changed row (rows from 1).</summary>
        public IReadOnlyList<string> DirtyPaths()
        {
            var paths = new List<string>();
            for (var i = 0; i < _rows.Count; i++)
            {
                var row = _rows[i];
                foreach (var pair in row.Values)
                {
                    if (pair.Value.IsDirty)
                    {
                        paths.Add(FieldPath.OfRow(Field.Name, i + 1, pair.Key));
                    }
                }
            }

            return paths;
        }

        /// <summary>Adds a new row at the end (its id is assigned by the caller when it
        /// needs one before save; new rows carry a null id). Journals the addition and
        /// raises one <see cref="StructureChangedEventArgs"/>.</summary>
        public RowDraft AddRow()
        {
            var row = new RowDraft(null);
            _rows.Add(row);
            Attach(row);
            ConfigureRow?.Invoke(row, _rows.Count);
            RowAdded?.Invoke(row);
            Record(new InsertStep(row, _rows.Count - 1, StructureChangeKind.RowAdded), "add row");
            NotifyStructure(new StructureChangedEventArgs(
                StructureChangeKind.RowAdded, Field.Name, rowKeys: new[] { row.Id }, index: _rows.Count - 1, count: 1));
            return row;
        }

        /// <summary>Adds a new row at the given position (0-based index; rows after it
        /// move down). Journals the insertion and raises one <see cref="StructureChangedEventArgs"/>.</summary>
        public RowDraft AddRowAt(int index)
        {
            var row = new RowDraft(null);
            var at = Math.Clamp(index, 0, _rows.Count);
            _rows.Insert(at, row);
            Attach(row);
            RefreshPermissions();
            RowAdded?.Invoke(row);
            Record(new InsertStep(row, at, StructureChangeKind.RowInserted), "insert row");
            NotifyStructure(new StructureChangedEventArgs(
                StructureChangeKind.RowInserted, Field.Name, rowKeys: new[] { row.Id }, index: at, count: 1));
            return row;
        }

        private void Attach(RowDraft row) => row.Owner = this;

        /// <summary>Removes the row at the given position (0-based). The journal keeps the
        /// row itself, so an undo brings it back with its id, values and dirty flags at the
        /// position it had.</summary>
        public void RemoveRowAt(int index)
        {
            var row = _rows[index];
            _rows.RemoveAt(index);
            RefreshPermissions();
            Record(new RemoveStep(row, index), "delete row");
            NotifyStructure(new StructureChangedEventArgs(
                StructureChangeKind.RowRemoved, Field.Name, rowKeys: new[] { row.Id }, index: index, count: 1));
        }

        /// <summary>Moves the row at <paramref name="from"/> (0-based) to
        /// <paramref name="to"/>; the contract's idx follows the new order. The journal
        /// records where the row came from.</summary>
        public void MoveRow(int from, int to)
        {
            var row = _rows[from];
            _rows.RemoveAt(from);
            var at = Math.Clamp(to, 0, _rows.Count);
            _rows.Insert(at, row);
            RefreshPermissions();
            Record(new MoveStep(row, from, at), "move row");
            NotifyStructure(new StructureChangedEventArgs(
                StructureChangeKind.RowMoved, Field.Name, rowKeys: new[] { row.Id }, index: at, count: 1, fromIndex: from));
        }

        /// <summary>Loads a decoded table value as the table's content and original. The
        /// journal is cleared (its originals are gone) and one
        /// <see cref="StructureChangedEventArgs"/> reports the load.</summary>
        public void LoadRows(TableFieldValue value)
        {
            ArgumentNullException.ThrowIfNull(value);
            _quiet++;
            _applying++;
            try
            {
                _rows.Clear();
                foreach (var row in value.Rows)
                {
                    var draft = new RowDraft(row.Id) { Owner = this };
                    foreach (var pair in row.Values)
                    {
                        draft.SetValue(pair.Key, pair.Value);
                        // The loaded value is the original: a just-loaded table is clean
                        // (the brief's LoadFrom contract) — without this every loaded row
                        // state sat dirty over its null-field default and every patch
                        // replaced the table in full.
                        draft.Values[pair.Key].MarkSaved();
                    }

                    ConfigureRow?.Invoke(draft, _rows.Count + 1);
                    _rows.Add(draft);
                }

                _originalRows = Snapshot(_rows);
                ClearJournal();
            }
            finally
            {
                _applying--;
                _quiet--;
            }

            NotifyStructure(new StructureChangedEventArgs(
                StructureChangeKind.RowsLoaded,
                Field.Name,
                rowKeys: _rows.Select(row => row.Id).ToArray(),
                index: 0,
                count: _rows.Count));
        }

        /// <summary>Reverts the rows to the loaded ones (ids, order and values). The
        /// journal is cleared and one <see cref="StructureChangedEventArgs"/> reports the revert.</summary>
        public void Revert()
        {
            _quiet++;
            _applying++;
            try
            {
                _rows.Clear();
                foreach (var row in _originalRows)
                {
                    var draft = new RowDraft(row.Id) { Owner = this };
                    foreach (var pair in row.Values)
                    {
                        draft._values[pair.Key] = pair.Value.SavedCopy();
                    }

                    _rows.Add(draft);
                }

                ClearJournal();
            }
            finally
            {
                _applying--;
                _quiet--;
            }

            NotifyStructure(new StructureChangedEventArgs(StructureChangeKind.Reverted, Field.Name));
        }

        /// <summary>Marks the current rows as the saved ones. The journal is cleared (its
        /// originals were just replaced) and one <see cref="StructureChangedEventArgs"/> reports it.</summary>
        public void MarkSaved()
        {
            _quiet++;
            _applying++;
            try
            {
                foreach (var row in _rows)
                {
                    foreach (var state in row.Values.Values) { state.MarkSaved(); state.ClearRevealed(); }
                }

                _originalRows = Snapshot(_rows);
                ClearJournal();
            }
            finally
            {
                _applying--;
                _quiet--;
            }

            NotifyStructure(new StructureChangedEventArgs(StructureChangeKind.Saved, Field.Name));
        }

        private static List<RowSnapshot> Snapshot(List<RowDraft> rows)
        {
            var snapshots = new List<RowSnapshot>(rows.Count);
            foreach (var row in rows)
            {
                snapshots.Add(new RowSnapshot(row.Id, row.SavedValues()));
            }

            return snapshots;
        }

        /// <summary>Assigns the id of a row the caller created (after the server names
        /// a new row); answers false when the row is not in this table.</summary>
        public bool AssignRowId(RowDraft row, string id)
        {
            if (!_rows.Contains(row))
            {
                return false;
            }

            row.AssignId(id);
            return true;
        }

        /// <summary>
        /// Writes the table's merge-patch value under the table's name: the full
        /// replacement row array — ids kept on rows the server knows, absent on new
        /// rows, with each row's <c>idx</c> following the new order (1-based). A row the
        /// user removed is simply absent, which the server reads as a delete.
        /// </summary>
        public void WritePatch(Utf8JsonWriter writer, string name)
        {
            writer.WritePropertyName(name);
            writer.WriteStartArray();
            foreach (var row in _rows)
            {
                writer.WriteStartObject();
                if (row.Id is { Length: > 0 } id)
                {
                    writer.WriteString("id", id);
                }

                writer.WriteNumber("idx", _rows.IndexOf(row) + 1);
                foreach (var pair in row.Values)
                {
                    if (!pair.Value.IsMasked && pair.Value.IsDirty)
                    {
                        ValueCodec.WriteScalar(writer, pair.Key, pair.Value.Value);
                    }
                }

                writer.WriteEndObject();
            }

            writer.WriteEndArray();
        }

        /// <summary>The editable state of one row.</summary>
        public sealed class RowDraft
        {
            /// <summary>Creates a row draft; the id is null for a new row.</summary>
            public RowDraft(string? id)
            {
                Id = id;
            }

            /// <summary>The table the row belongs to (null while it is being built).</summary>
            internal TableDraft? Owner { get; set; }

            /// <summary>The row's server id, or null while the row is new.</summary>
            public string? Id { get; private set; }

            /// <summary>True while the server has not named the row.</summary>
            public bool IsNew => Id is null;

            /// <summary>The row's fields' states.</summary>
            public IReadOnlyDictionary<string, FieldState> Values => _values;

            internal readonly Dictionary<string, FieldState> _values = new(StringComparer.Ordinal);

            /// <summary>
            /// The errors attached to the row itself — the contract's <c>table[idx]</c> path
            /// (a row-level server error), kept beside the cell errors in
            /// <see cref="Values"/>. Keyed by nothing but the row, so a reorder or a delete
            /// above it re-keys nothing.
            /// </summary>
            public IReadOnlyList<FieldError> RowErrors => _rowErrors;

            private readonly List<FieldError> _rowErrors = new();

            internal void EnsureField(string field) => _values.TryAdd(field, new FieldState(FieldValue.Null));

            /// <summary>Sets (or replaces) the current value of a row field. The edit is
            /// journaled, raises <see cref="DocumentDraft.FieldChanged"/> and drops the
            /// cell's server error.</summary>
            public void SetValue(string field, FieldValue value)
            {
                var hadField = _values.TryGetValue(field, out var state);
                var before = state?.Value;
                var applied = value ?? FieldValue.Null;
                if (hadField)
                {
                    state!.Value = applied;
                }
                else
                {
                    _values[field] = new FieldState(FieldValue.Null) { Value = applied };
                }

                Owner?.OnRowValueSet(this, field, before, applied, hadField);
            }

            /// <summary>Replaces the row-level errors and reports the change.</summary>
            public void SetRowErrors(IEnumerable<FieldError> errors)
            {
                ArgumentNullException.ThrowIfNull(errors);
                _rowErrors.Clear();
                _rowErrors.AddRange(errors);
                Owner?.NotifyRowValidation(this, null);
            }

            /// <summary>Clears the row-level errors (reports the change when there were any).</summary>
            public void ClearRowErrors()
            {
                if (_rowErrors.Count == 0)
                {
                    return;
                }

                _rowErrors.Clear();
                Owner?.NotifyRowValidation(this, null);
            }

            /// <summary>Gets the current value of a row field (null when absent).</summary>
            public FieldValue? GetValue(string field) =>
                _values.TryGetValue(field, out var state) ? state.Value : null;

            /// <summary>The value the row was loaded with, per field.</summary>
            internal IReadOnlyDictionary<string, FieldState> SavedValues()
            {
                var values = new Dictionary<string, FieldState>(StringComparer.Ordinal);
                foreach (var pair in _values)
                {
                    values[pair.Key] = pair.Value.SavedCopy();
                }

                return values;
            }

            /// <summary>Names a row the server has created.</summary>
            internal void AssignId(string id) => Id = id;
        }

        private sealed record RowSnapshot(string? Id, IReadOnlyDictionary<string, FieldState> Values);
    }
}