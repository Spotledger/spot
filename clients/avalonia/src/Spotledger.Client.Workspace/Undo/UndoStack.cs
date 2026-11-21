using Avalonia.Threading;
using Spotledger.Client.ChildTable;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Workspace;

/// <summary>
/// The undo and redo history of one open form. It watches the draft's own events, so the form's
/// editors keep calling the draft directly (no change to Forms): everything the draft reports
/// between two turns of the UI is one <b>user edit</b>, and the field changes an instant rule or a
/// server form-event diff made in that same turn are part of it, undone and redone with it through a
/// before/after snapshot of the scalar fields. Child-row changes (add, insert, delete, move, cell edit,
/// paste, bulk fill) replay through the table's own row journal, one journal entry per recorded row
/// operation, so ids, values and positions come back exactly.
/// </summary>
/// <remarks>
/// Typing in one field within 1 second merges into one entry. A new edit drops the redo tail. The
/// history never crosses a save, a reload or a revert (it clears, with a note the UI shows) and is
/// bounded (200 steps by default), so ten thousand edits keep memory flat. If something other than
/// this stack replays a table's journal (the table control's own undo), the history clears rather than
/// guess.
///
/// <b>The gate.</b> Every undo and redo is checked before anything changes: the document state
/// (<see cref="UndoGate.DocumentRefusal"/>), then every table and every header field the step touches,
/// through the checks the host supplied with <see cref="SetGate"/>. A refusal changes nothing, leaves
/// the step where it is and is returned. Nothing defaults to allow: with no gate supplied a step that
/// touches a child table is refused whole, and a header field is checked against the draft's own field
/// state (masked, hidden, read-only or non-editable refuses).
/// </remarks>
[System.Diagnostics.CodeAnalysis.SuppressMessage("Naming", "CA1711:Identifiers should not have incorrect suffix", Justification = "UndoStack is the name the task brief gives; it is a history, not a collection.")]
public sealed class UndoStack : IDisposable
{
    /// <summary>The default bound on the history.</summary>
    public const int DefaultBound = 200;

    private static readonly TimeSpan CoalesceWindow = TimeSpan.FromSeconds(1);

    private readonly DocumentDraft _draft;
    private readonly TimeProvider _clock;
    private readonly int _bound;
    private readonly Action<Action> _schedule;
    private readonly List<Step> _steps = [];
    private readonly Dictionary<string, FieldValue> _shadow = new(StringComparer.Ordinal);
    private int _cursor;
    private int _applying;
    private bool _pending;
    private bool _mergeNext;
    private string? _firstLabel;
    private readonly Dictionary<string, int> _pendingRows = new(StringComparer.Ordinal);
    private Func<string, ChildTableResult?> _tableGate;
    private Func<string, ChildTableResult?> _fieldGate;
    private (string Table, int Row)? _replayTarget;
    private bool _disposed;

    /// <summary>Creates the stack over a form's draft and starts watching it.</summary>
    /// <param name="draft">The form's draft.</param>
    /// <param name="clock">The clock for coalescing typing.</param>
    /// <param name="bound">The most steps kept.</param>
    /// <param name="schedule">Closes a user edit at the end of the UI turn (default: the UI thread's background priority). Tests pass a no-op and call <see cref="Flush"/>.</param>
    public UndoStack(DocumentDraft draft, TimeProvider? clock = null, int bound = DefaultBound, Action<Action>? schedule = null)
    {
        ArgumentNullException.ThrowIfNull(draft);
        ArgumentOutOfRangeException.ThrowIfLessThan(bound, 1);
        _draft = draft;
        _clock = clock ?? TimeProvider.System;
        _bound = bound;
        _schedule = schedule ?? (action => Dispatcher.UIThread.Post(action, DispatcherPriority.Background));
        // Defaults fail closed: no gate call means no table may change, and a header field is checked
        // against the draft's own state.
        _tableGate = RefuseEveryTable;
        _fieldGate = FieldGateFromDraft;
        foreach (var table in draft.Tables.Values)
        {
            // The row journal must outlive every step this stack keeps.
            table.JournalDepth = Math.Max(table.JournalDepth, bound * 8);
        }

        Resync();
        draft.FieldChanged += OnFieldChanged;
        draft.StructureChanged += OnStructureChanged;
    }

    /// <summary>Raised when the history, a label or the availability changed.</summary>
    public event EventHandler? Changed;

    /// <summary>Raised when the history was cleared by a save, reload or revert; the argument is the note to show.</summary>
    public event EventHandler<string>? Cleared;

    /// <summary>How many steps are kept (undo and redo together).</summary>
    public int Count
    {
        get
        {
            Flush();
            return _steps.Count;
        }
    }

    /// <summary>True when there is a step to undo.</summary>
    public bool CanUndo
    {
        get
        {
            Flush();
            return _cursor > 0;
        }
    }

    /// <summary>True when there is a step to redo.</summary>
    public bool CanRedo
    {
        get
        {
            Flush();
            return _cursor < _steps.Count;
        }
    }

    /// <summary>The name of the step Undo would reverse ("Edit Quantity"), or null.</summary>
    public string? UndoLabel
    {
        get
        {
            Flush();
            return _cursor > 0 ? _steps[_cursor - 1].Label : null;
        }
    }

    /// <summary>The name of the step Redo would replay, or null.</summary>
    public string? RedoLabel
    {
        get
        {
            Flush();
            return _cursor < _steps.Count ? _steps[_cursor].Label : null;
        }
    }

    /// <summary>The note of the last clear ("Undo history cleared after save."), or null.</summary>
    public string? Note { get; private set; }

    /// <summary>Closes the user edit in progress (the UI turn ended): records it as one step.</summary>
    public void Flush()
    {
        if (!_pending || _applying > 0)
        {
            return;
        }

        _pending = false;
        var merge = _mergeNext;
        var scalars = DiffScalars();
        var rows = new Dictionary<string, int>(_pendingRows, StringComparer.Ordinal);
        var label = _firstLabel;
        _pendingRows.Clear();
        _firstLabel = null;
        if (scalars.Count == 0 && rows.Count == 0)
        {
            return;
        }

        if (merge && _cursor > 0 && _cursor == _steps.Count)
        {
            _steps[_cursor - 1].Merge(scalars, rows);
            RaiseChanged();
            return;
        }

        var now = _clock.GetUtcNow();
        if (rows.Count == 0 && scalars.Count == 1 && _cursor > 0 && _cursor == _steps.Count)
        {
            var top = _steps[_cursor - 1];
            if (top.IsSingleField(scalars[0].Name) && now - top.At <= CoalesceWindow)
            {
                top.Merge(scalars, rows);
                top.At = now;
                if (top.IsNoOp)
                {
                    _steps.RemoveAt(--_cursor);
                }

                RaiseChanged();
                return;
            }
        }

        if (_cursor < _steps.Count)
        {
            _steps.RemoveRange(_cursor, _steps.Count - _cursor);
        }

        _steps.Add(new Step(label ?? "Edit", scalars, rows, now));
        _cursor = _steps.Count;
        if (_steps.Count > _bound)
        {
            _steps.RemoveRange(0, _steps.Count - _bound);
            _cursor = _steps.Count;
        }

        RaiseChanged();
    }

    /// <summary>
    /// Runs a change the user did not make by hand (a server form-event diff applied to the draft) and
    /// folds what it changed into the last step, so one Undo reverses the edit and its consequences.
    /// </summary>
    public void RunDerived(Action change)
    {
        ArgumentNullException.ThrowIfNull(change);
        Flush();
        _mergeNext = true;
        try
        {
            change();
            _pending = true;
            Flush();
        }
        finally
        {
            _mergeNext = false;
        }
    }

    /// <summary>Reverses the last step. False unless a step was applied (nothing to undo, or refused).</summary>
    public bool Undo() => TryUndo().Succeeded;

    /// <summary>Replays the last undone step. False unless a step was applied.</summary>
    public bool Redo() => TryRedo().Succeeded;

    /// <summary>
    /// Reverses the last step and says what happened: applied, nothing to undo, or the gate's refusal.
    /// The gate is asked for the document, then for every table and field the step touches, before
    /// anything changes; a refusal leaves the step in place.
    /// </summary>
    public ChildTableResult TryUndo()
    {
        Flush();
        if (_cursor == 0)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "There is nothing to undo.");
        }

        var step = _steps[_cursor - 1];
        if (Refusal(step) is { } refused)
        {
            return refused;
        }

        if (!Apply(step, undo: true))
        {
            return ChildTableResult.Refused(ChildTableRefusal.Invalid, "The step could not be replayed, so the undo history was cleared.");
        }

        _cursor--;
        RaiseChanged();
        return Outcome(step);
    }

    /// <summary>Replays the last undone step and says what happened (applied, nothing to redo, or a refusal).</summary>
    public ChildTableResult TryRedo()
    {
        Flush();
        if (_cursor >= _steps.Count)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "There is nothing to redo.");
        }

        var step = _steps[_cursor];
        if (Refusal(step) is { } refused)
        {
            return refused;
        }

        if (!Apply(step, undo: false))
        {
            return ChildTableResult.Refused(ChildTableRefusal.Invalid, "The step could not be replayed, so the undo history was cleared.");
        }

        _cursor++;
        RaiseChanged();
        return Outcome(step);
    }

    /// <summary>The row the last replay landed on, when it landed on one (the adapter places a cursor with it).</summary>
    internal (string Table, int Row)? LastReplayTarget => _replayTarget;

    /// <summary>
    /// Supplies the checks that decide whether a step may be undone or redone now. The host answers
    /// with its own gate: <paramref name="table"/> returns null when the table may change (normally
    /// <see cref="ChildTableModel.CheckEditable"/> of that table) or the refusal, and so does
    /// <paramref name="field"/> for a header field. A null <paramref name="field"/> keeps the default,
    /// which reads the draft's own field state; nothing defaults to allow.
    /// </summary>
    /// <param name="table">The check for a child table, by table field name.</param>
    /// <param name="field">The check for a header field, by field name; null reads the draft's state.</param>
    public void SetGate(Func<string, ChildTableResult?> table, Func<string, ChildTableResult?>? field = null)
    {
        ArgumentNullException.ThrowIfNull(table);
        _tableGate = table;
        _fieldGate = field ?? FieldGateFromDraft;
    }

    /// <summary>
    /// Every check a step needs, run before any change: the document state first, then every table and
    /// every field the step touches. Null when the step may run.
    /// </summary>
    private ChildTableResult? Refusal(Step step)
    {
        if (UndoGate.DocumentRefusal(_draft) is { } document)
        {
            return document;
        }

        foreach (var table in step.Rows.Keys)
        {
            // A step on a table that is no longer on the document cannot change anything.
            if (_draft.Tables.ContainsKey(table) && _tableGate(table) is { } tableRefusal)
            {
                return tableRefusal;
            }
        }

        foreach (var scalar in step.Scalars)
        {
            if (_fieldGate(scalar.Name) is { } fieldRefusal)
            {
                return fieldRefusal;
            }
        }

        return null;
    }

    /// <summary>What a step that ran reports: the row it landed on, so the table's cursor follows it.</summary>
    private ChildTableResult Outcome(Step step)
    {
        if (step.Rows.Count != 1 || _replayTarget is not { } target || !step.Rows.ContainsKey(target.Table))
        {
            // A header-only step (or one across several tables) names no row: the table keeps its cursor.
            return ChildTableResult.Ok();
        }

        return ChildTableResult.Ok((Math.Max(0, target.Row), -1), [Math.Max(0, target.Row)]);
    }

    private static ChildTableResult? RefuseEveryTable(string table)
        => ChildTableResult.Refused(
            ChildTableRefusal.NoWriteRight,
            $"The form has given no edit check for the table {table}, so the change is refused.");

    /// <summary>The default header-field check: the draft's own state, which is the same state the
    /// controls read. Masked, hidden, read-only and non-editable fields refuse.</summary>
    private ChildTableResult? FieldGateFromDraft(string name)
    {
        if (!_draft.Fields.TryGetValue(name, out var state))
        {
            return ChildTableResult.Refused(ChildTableRefusal.OutOfRange, $"The field {name} is no longer on this document.");
        }

        if (state.IsMasked)
        {
            return ChildTableResult.Refused(ChildTableRefusal.CellMasked, $"The field {name} is masked for you, so the change is refused.");
        }

        if (!state.IsVisible)
        {
            return ChildTableResult.Refused(ChildTableRefusal.CellHidden, $"The field {name} is hidden, so the change is refused.");
        }

        return state.IsEditable && !state.IsReadOnly
            ? null
            : ChildTableResult.Refused(ChildTableRefusal.CellReadOnly, $"The field {name} is read-only, so the change is refused.");
    }

    /// <summary>Forgets the whole history and records why.</summary>
    public void Clear(string note)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(note);
        _steps.Clear();
        _cursor = 0;
        _pending = false;
        _pendingRows.Clear();
        _firstLabel = null;
        _replayTarget = null;
        Note = note;
        Resync();
        Cleared?.Invoke(this, note);
        RaiseChanged();
    }

    /// <inheritdoc />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _draft.FieldChanged -= OnFieldChanged;
        _draft.StructureChanged -= OnStructureChanged;
    }

    // ------------------------------------------------------------------ replay

    private bool Apply(Step step, bool undo)
    {
        _replayTarget = null;
        _applying++;
        try
        {
            foreach (var (table, count) in step.Rows)
            {
                if (!_draft.Tables.TryGetValue(table, out var draftTable))
                {
                    continue;
                }

                for (var i = 0; i < count; i++)
                {
                    if (!(undo ? draftTable.UndoRow() : draftTable.RedoRow()))
                    {
                        _applying--;
                        try
                        {
                            Clear("Undo history cleared: a table change could not be replayed.");
                        }
                        finally
                        {
                            _applying++;
                        }

                        return false;
                    }
                }
            }

            ApplyScalars(step, undo);
        }
        finally
        {
            _applying--;
        }

        Resync();
        return true;
    }

    private void ApplyScalars(Step step, bool undo)
    {
        var targets = step.Scalars.Select(s => (s.Name, Value: undo ? s.Before : s.After)).ToList();
        if (undo)
        {
            targets.Reverse();
        }

        // The edited field first (its rules re-run), then force every field to the recorded value.
        foreach (var (name, value) in targets)
        {
            SetIfDifferent(name, value);
        }

        foreach (var (name, value) in targets)
        {
            SetIfDifferent(name, value);
        }
    }

    private void SetIfDifferent(string name, FieldValue value)
    {
        if (!_draft.Fields.TryGetValue(name, out var state) || state.IsMasked || SameValue(state.Value, value))
        {
            return;
        }

        _draft.Set(name, value);
    }

    // ------------------------------------------------------------------ watching

    private void OnFieldChanged(object? sender, string path)
    {
        if (_applying > 0 || _disposed)
        {
            return;
        }

        var bracket = path.IndexOf('[', StringComparison.Ordinal);
        if (bracket < 0)
        {
            Begin("Edit " + (_draft.Model.Field(path)?.Label ?? path));
            return;
        }

        var table = path[..bracket];
        var dot = path.LastIndexOf('.');
        var cell = dot > bracket ? path[(dot + 1)..] : null;
        var rowModel = _draft.Model.ChildTable(table);
        Begin(cell is null ? "Edit row" : "Edit " + (rowModel?.Field(cell)?.Label ?? cell));
    }

    private void OnStructureChanged(object? sender, StructureChangedEventArgs e)
    {
        if (_disposed)
        {
            return;
        }

        if (_applying > 0 && (e.IsUndo || e.IsRedo) && e.Table is { } replayed)
        {
            // Remember where the replay landed, so the table's cursor follows the step it undid.
            _replayTarget = (replayed, Math.Max(0, e.Index));
        }

        switch (e.Kind)
        {
            case StructureChangeKind.DocumentLoaded or StructureChangeKind.RowsLoaded:
                if (_applying == 0)
                {
                    Clear("Undo history cleared: the document was reloaded.");
                }

                return;
            case StructureChangeKind.Saved:
                if (_applying == 0)
                {
                    Clear("Undo history cleared after save.");
                }

                return;
            case StructureChangeKind.Reverted:
                if (_applying == 0)
                {
                    Clear("Undo history cleared: the changes were reverted.");
                }

                return;
            case StructureChangeKind.PermissionsApplied:
                Resync();
                return;
        }

        if (_applying > 0)
        {
            return;
        }

        if (e.IsUndo || e.IsRedo)
        {
            // Something other than this stack replayed a table's journal: the two no longer agree.
            Clear("Undo history cleared: a table was undone by its own control.");
            return;
        }

        if (e.Table is { } table)
        {
            _pendingRows[table] = _pendingRows.GetValueOrDefault(table) + 1;
        }

        Begin(e.Kind switch
        {
            StructureChangeKind.RowAdded => "Add row",
            StructureChangeKind.RowInserted => "Insert row",
            StructureChangeKind.RowRemoved => "Delete row",
            StructureChangeKind.RowMoved => "Move row",
            StructureChangeKind.CellEdited => "Edit " + (CellLabel(e) ?? "cell"),
            StructureChangeKind.BulkOperation => Capitalise(e.Description ?? "Bulk edit"),
            _ => "Edit",
        });
    }

    private string? CellLabel(StructureChangedEventArgs e)
        => e.Table is { } table && e.Field is { } field ? _draft.Model.ChildTable(table)?.Field(field)?.Label ?? field : null;

    private static string Capitalise(string text) => text.Length == 0 ? text : char.ToUpperInvariant(text[0]) + text[1..];

    private void Begin(string label)
    {
        _firstLabel ??= label;
        if (_pending)
        {
            return;
        }

        _pending = true;
        _schedule(Flush);
    }

    // ------------------------------------------------------------------ snapshots

    private void Resync()
    {
        _shadow.Clear();
        foreach (var field in _draft.Model.Fields)
        {
            if (field.Kind != FieldKind.Table && _draft.Fields.TryGetValue(field.Name, out var state) && !state.IsMasked)
            {
                _shadow[field.Name] = state.Value;
            }
        }
    }

    private List<ScalarChange> DiffScalars()
    {
        var changes = new List<ScalarChange>();
        foreach (var field in _draft.Model.Fields)
        {
            if (field.Kind == FieldKind.Table || !_draft.Fields.TryGetValue(field.Name, out var state) || state.IsMasked)
            {
                continue;
            }

            var before = _shadow.GetValueOrDefault(field.Name) ?? FieldValue.Null;
            if (!SameValue(before, state.Value))
            {
                changes.Add(new ScalarChange(field.Name, before, state.Value));
            }

            _shadow[field.Name] = state.Value;
        }

        return changes;
    }

    internal static bool SameValue(FieldValue a, FieldValue b) => (a, b) switch
    {
        (JsonFieldValue x, JsonFieldValue y) => x.SemanticEquals(y),
        (MultiSelectFieldValue x, MultiSelectFieldValue y) => x.Values.SequenceEqual(y.Values),
        _ => a.Equals(b),
    };

    private void RaiseChanged() => Changed?.Invoke(this, EventArgs.Empty);

    private sealed record ScalarChange(string Name, FieldValue Before, FieldValue After);

    private sealed class Step
    {
        private readonly List<ScalarChange> _scalars;
        private readonly Dictionary<string, int> _rows;

        public Step(string label, List<ScalarChange> scalars, Dictionary<string, int> rows, DateTimeOffset at)
        {
            Label = label;
            _scalars = scalars;
            _rows = rows;
            At = at;
        }

        public string Label { get; }
        public DateTimeOffset At { get; set; }
        public IReadOnlyList<ScalarChange> Scalars => _scalars;
        public IReadOnlyDictionary<string, int> Rows => _rows;

        public bool IsSingleField(string name) => _rows.Count == 0 && _scalars.Count == 1 && _scalars[0].Name == name;

        public bool IsNoOp => _rows.Count == 0 && _scalars.Count == 0;

        public void Merge(List<ScalarChange> more, Dictionary<string, int> rows)
        {
            foreach (var change in more)
            {
                var index = _scalars.FindIndex(s => s.Name == change.Name);
                if (index < 0)
                {
                    _scalars.Add(change);
                }
                else if (SameValue(_scalars[index].Before, change.After))
                {
                    _scalars.RemoveAt(index);
                }
                else
                {
                    _scalars[index] = _scalars[index] with { After = change.After };
                }
            }

            foreach (var (table, count) in rows)
            {
                _rows[table] = _rows.GetValueOrDefault(table) + count;
            }
        }
    }
}
