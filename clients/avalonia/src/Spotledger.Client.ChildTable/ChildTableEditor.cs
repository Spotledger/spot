using Avalonia.Controls;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.ChildTable;

/// <summary>
/// The grid's cell editor factory: the field control of the cell's field, from
/// <see cref="FieldControlFactory"/> (the only field-kind dispatch), hosted compactly in the
/// cell. A link cell gets the lookup box with a source built from the row's context, a decimal
/// cell right-aligns and selects its text on entry, a select opens on F4 or Alt+Down (the combo
/// box's own keys).
///
/// An editor opens only for a cell the model allows, and a commit is checked again: the right,
/// the document state or a permission may change while the editor is open. A commit goes through
/// <see cref="ChildTableModel.SetCell"/>, so it reaches the draft through the row's journaled
/// <c>SetValue</c> and never through the control's own write path.
/// </summary>
public sealed class ChildTableEditor : ICellEditorFactory
{
    private readonly ChildTableModel _model;
    private readonly Func<ChildTableLookupRequest, ILookupSource?>? _lookups;
    private readonly ILookupDelay? _lookupDelay;

    /// <summary>Creates the factory.</summary>
    /// <param name="model">The table model.</param>
    /// <param name="lookups">Builds the lookup source of a link cell from the row's context
    /// (the row snapshot is the lookup's <c>doc</c>, the document the <c>parent</c>); null leaves
    /// link cells without a source.</param>
    /// <param name="lookupDelay">The lookup's type-ahead delay; null keeps the control's own.</param>
    public ChildTableEditor(
        ChildTableModel model,
        Func<ChildTableLookupRequest, ILookupSource?>? lookups = null,
        ILookupDelay? lookupDelay = null)
    {
        ArgumentNullException.ThrowIfNull(model);
        _model = model;
        _lookups = lookups;
        _lookupDelay = lookupDelay;
    }

    /// <summary>Raised when an open editor's key asks to leave the cell.</summary>
    public event EventHandler<ChildTableEditorExitEventArgs>? ExitRequested;

    /// <inheritdoc />
    public Control? CreateEditor(int rowIndex, int columnIndex, GridRow row, GridColumnModel column)
    {
        if (_model.CellRefusal(rowIndex, columnIndex) != ChildTableRefusal.None)
        {
            return null;
        }

        var field = _model.Columns[columnIndex].Field;
        var draftRow = _model.Table.Rows[rowIndex];
        var state = draftRow.Values[field.Name];
        var host = TakeHost(columnIndex, field);
        var control = host.Editor;
        var binding = FieldControlFactory.GetBinding(control);
        binding.Load(state, _model.Draft.Currency.GetValueOrDefault(FieldPath.OfRow(_model.TableName, rowIndex + 1, field.Name)));
        if (control is SpotLookupBox lookup)
        {
            lookup.Source = _lookups?.Invoke(new ChildTableLookupRequest(
                field, _model.TableName, rowIndex, _model.Snapshot(rowIndex), _model.Draft));
            if (_lookupDelay is not null)
            {
                lookup.Delay = _lookupDelay;
            }
        }

        // The scratch state is what the control commits into; the real state changes only through SetCell.
        host.Bind(draftRow, rowIndex, new FieldState(state.Value, field));
        return host;
    }

    private readonly Dictionary<int, CellEditorHost> _idle = [];

    /// <summary>A rested host for the column, or a new one. Building a field control costs more than a
    /// key may take, so a column's host is reused for every cell edit in it.</summary>
    private CellEditorHost TakeHost(int columnIndex, FieldModel field)
    {
        if (_idle.Remove(columnIndex, out var idle))
        {
            return idle;
        }

        CellEditorHost? created = null;
        created = new CellEditorHost(FieldControlFactory.Create(field), field, columnIndex,
            exit => ExitRequested?.Invoke(this, new ChildTableEditorExitEventArgs(exit, created!.RowIndex, columnIndex)));
        return created;
    }

    /// <summary>Takes a finished editor back for the next edit in its column. Called when the grid
    /// has closed the edit.</summary>
    internal void Recycle(CellEditorHost host)
    {
        host.Reset();
        _idle[host.Column] = host;
    }

    /// <inheritdoc />
    public string GetValue(Control editor)
    {
        var host = (CellEditorHost)editor;
        var binding = FieldControlFactory.GetBinding(host.Editor);
        var ok = binding.CommitTo(host.Scratch);
        return ok ? CellFormatter.Format(host.Field, host.Scratch.Value) : string.Empty;
    }

    /// <inheritdoc />
    public bool TryCommit(int rowIndex, int columnIndex, string proposedValue, Control editor)
    {
        var host = (CellEditorHost)editor;
        var scratch = host.Scratch;
        if (scratch.Errors.Count > 0)
        {
            // The control refused the text (not a number, too many decimals, not an option):
            // the editor stays open and the cell says why.
            host.ShowError(scratch.Errors[0].Message);
            return false;
        }

        // The row may have moved while the editor was open; the write follows the row, never the index.
        var index = IndexOf(host);
        if (index < 0)
        {
            host.ShowError("This row is no longer in the table.");
            return false;
        }

        var refusal = _model.CellRefusal(index, columnIndex);
        if (refusal != ChildTableRefusal.None)
        {
            host.ShowError(ChildTableResult.Describe(refusal));
            return false;
        }

        if (scratch.IsDirty)
        {
            var result = _model.SetCell(index, columnIndex, scratch.Value);
            if (!result.Succeeded)
            {
                host.ShowError(result.Message);
                return false;
            }
        }

        host.ShowError(null);
        return true;
    }

    /// <inheritdoc />
    public void Cancel(int rowIndex, int columnIndex, Control editor)
    {
        // The edit is discarded; the host rests in Recycle when the grid reports the edit finished.
    }

    private int IndexOf(CellEditorHost host)
    {
        var rows = _model.Table.Rows;
        for (var i = 0; i < rows.Count; i++)
        {
            if (ReferenceEquals(rows[i], host.Row))
            {
                return i;
            }
        }

        return -1;
    }
}
