using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using RowDraft = Spotledger.Client.Metadata.DocumentDraft.TableDraft.RowDraft;

namespace Spotledger.Client.ChildTable;

/// <summary>How a cell editor asks to be left.</summary>
public enum ChildTableEditorExit
{
    /// <summary>Enter: commit and go down; on the last row's last cell, add a row.</summary>
    Enter,

    /// <summary>Ctrl+Enter: commit and stay.</summary>
    CtrlEnter,

    /// <summary>Tab: commit and go to the next editable cell.</summary>
    Tab,

    /// <summary>Shift+Tab: commit and go to the previous editable cell.</summary>
    ShiftTab,

    /// <summary>Up: commit and go up.</summary>
    Up,

    /// <summary>Down: commit and go down.</summary>
    Down,

    /// <summary>Escape with no list open: discard the edit.</summary>
    Cancel,
}

/// <summary>Raised by an open cell editor when a key asks to leave the cell.</summary>
public sealed class ChildTableEditorExitEventArgs(ChildTableEditorExit exit, int row, int column) : EventArgs
{
    /// <summary>How the editor asks to be left.</summary>
    public ChildTableEditorExit Exit { get; } = exit;

    /// <summary>The 0-based row being edited.</summary>
    public int Row { get; } = row;

    /// <summary>The column being edited.</summary>
    public int Column { get; } = column;
}

/// <summary>
/// The control a cell hosts while it is edited: one field control from
/// <see cref="FieldControlFactory"/>, a scratch <see cref="FieldState"/> the control commits
/// into (the real state is only ever changed through the row's journaled <c>SetValue</c>), and
/// the keys that leave the cell. A key the field control does not use never reaches the grid
/// underneath: the grid's own arrow, space and type-ahead handling must not act on a cell that is
/// being edited.
/// </summary>
public sealed class CellEditorHost : Decorator
{
    private readonly Action<ChildTableEditorExit> _exit;
    private readonly bool _numeric;
    private readonly bool _selectOnEntry;
    private bool _focusedOnce;

    internal CellEditorHost(Control field, FieldModel model, int column, Action<ChildTableEditorExit> exit)
    {
        Child = field;
        Field = model;
        Column = column;
        _exit = exit;
        _numeric = model.Kind is FieldKind.DecimalNumber or FieldKind.IntegerNumber;
        _selectOnEntry = _numeric || model.Kind is FieldKind.Link or FieldKind.DynamicLink;
        Focusable = true;
        field.MinHeight = 0;
        field.VerticalAlignment = Avalonia.Layout.VerticalAlignment.Stretch;
        // The exit keys are the host's up front (tunnel): a text box would move its caret on Up and Down,
        // a button click on Enter, a date input's text box commit on Enter, a combo box open its list on
        // Enter. Only while a list is open (a lookup's, a drop-down's, a calendar) are Enter, Tab and the
        // arrows the control's: then it gets them first and the host takes what it leaves (bubble).
        AddHandler(KeyDownEvent, OnKeyTunnel, RoutingStrategies.Tunnel);
        AddHandler(KeyDownEvent, OnKeyBubble, RoutingStrategies.Bubble);
    }

    /// <summary>The field being edited.</summary>
    public FieldModel Field { get; }

    /// <summary>The draft row being edited (its identity, so a reorder cannot redirect the write).</summary>
    public RowDraft Row { get; private set; } = null!;

    /// <summary>The row's position when the editor opened.</summary>
    public int RowIndex { get; private set; }

    /// <summary>The column being edited.</summary>
    public int Column { get; }

    /// <summary>The state the field control commits into.</summary>
    internal FieldState Scratch { get; private set; } = null!;

    /// <summary>Points the host at a row for its next edit. A host is reused: building a field
    /// control (its template, its styles) costs more than a key may take.</summary>
    internal void Bind(RowDraft row, int rowIndex, FieldState scratch)
    {
        Row = row;
        RowIndex = rowIndex;
        Scratch = scratch;
        _focusedOnce = false;
    }

    /// <summary>Puts a used host back to rest: no message, no open list.</summary>
    internal void Reset()
    {
        ShowError(null);
        switch (Editor)
        {
            case SpotLookupBox lookup:
                lookup.Close(true);
                break;
            case ComboBox combo:
                combo.IsDropDownOpen = false;
                break;
            case SpotDecimalInput decimalInput:
                decimalInput.ErrorText = null;
                break;
            default:
                break;
        }
    }

    /// <summary>The message shown on the cell after a refused commit; null when none.</summary>
    public string? ErrorText { get; private set; }

    /// <summary>The field control inside the host.</summary>
    public Control Editor => Child!;

    /// <summary>Shows a message on the cell.</summary>
    internal void ShowError(string? message)
    {
        ErrorText = message;
        ToolTip.SetTip(Editor, message);
        ((IPseudoClasses)Editor.Classes).Set(":error", message is not null);
    }

    /// <inheritdoc />
    protected override void OnGotFocus(FocusChangedEventArgs e)
    {
        base.OnGotFocus(e);
        if (!ReferenceEquals(e.Source, this))
        {
            return;
        }

        EnsureTemplates(Editor);
        var inner = FocusTarget(Editor);
        if (inner is not null && !ReferenceEquals(inner, this) && !inner.Focus())
        {
            // A field control's inner text box joins the visual tree in its first layout pass.
            UpdateLayout();
            inner = FocusTarget(Editor);
            inner?.Focus();
        }

        if (!_focusedOnce)
        {
            _focusedOnce = true;
            if (inner is TextBox box)
            {
                box.TextAlignment = _numeric ? TextAlignment.Right : TextAlignment.Left;
                if (_selectOnEntry)
                {
                    box.SelectAll();
                }
                else
                {
                    box.CaretIndex = box.Text?.Length ?? 0;
                }
            }
        }
    }

    /// <summary>A templated field control (a text input, a date input) builds its inner text box when
    /// it is first laid out; the editor is asked for focus before that, so build it now.</summary>
    private static void EnsureTemplates(Control control)
    {
        if (control is TemplatedControl templated)
        {
            templated.ApplyTemplate();
        }

        foreach (var descendant in control.GetVisualDescendants().OfType<TemplatedControl>().ToList())
        {
            descendant.ApplyTemplate();
        }
    }

    private static Control? FocusTarget(Control control) => control switch
    {
        SpotLookupBox lookup => lookup.Editor,
        TextBox or ComboBox or CheckBox or ToggleButton => control,
        _ => control.Focusable
            ? control
            : control.GetVisualDescendants().OfType<Control>().FirstOrDefault(c => c is TextBox || (c.Focusable && c.IsEffectivelyVisible)),
    };

    private void OnKeyTunnel(object? sender, KeyEventArgs e)
    {
        if (e.Handled)
        {
            return;
        }

        if (e.Key == Key.Escape && e.KeyModifiers == KeyModifiers.None && !ListIsOpen)
        {
            // A lookup box takes Escape even with its list closed; the cell is still to be left.
            e.Handled = true;
            _exit(ChildTableEditorExit.Cancel);
            return;
        }

        if (ListIsOpen || (e.Key is Key.Up or Key.Down && Editor is SpotLookupBox or ComboBox))
        {
            return; // the list's keys: the control gets them first, the host takes what is left (bubble)
        }

        Exit(e);
    }

    private bool ListIsOpen => Editor switch
    {
        SpotLookupBox lookup => lookup.IsOpen,
        ComboBox combo => combo.IsDropDownOpen,
        SpotDateInput date => date.IsCalendarOpen,
        _ => false,
    };

    private void OnKeyBubble(object? sender, KeyEventArgs e)
    {
        if (e.Handled)
        {
            return;
        }

        if (!Exit(e) && e.Key != Key.Escape)
        {
            // Not the field control's and not an exit: it must not reach the grid. (Escape with a
            // list open was the control's; with none it was taken above.)
            e.Handled = true;
        }
    }

    /// <summary>Takes the key when it asks to leave the cell. Returns true when it did.</summary>
    private bool Exit(KeyEventArgs e)
    {
        var modifiers = e.KeyModifiers & ~KeyModifiers.Shift;
        ChildTableEditorExit exit;
        switch (e.Key)
        {
            case Key.Enter when modifiers == KeyModifiers.None:
                exit = ChildTableEditorExit.Enter;
                break;
            case Key.Enter when modifiers == KeyModifiers.Control:
                exit = ChildTableEditorExit.CtrlEnter;
                break;
            case Key.Tab when modifiers == KeyModifiers.None:
                exit = e.KeyModifiers.HasFlag(KeyModifiers.Shift) ? ChildTableEditorExit.ShiftTab : ChildTableEditorExit.Tab;
                break;
            case Key.Up when e.KeyModifiers == KeyModifiers.None:
                exit = ChildTableEditorExit.Up;
                break;
            case Key.Down when e.KeyModifiers == KeyModifiers.None:
                exit = ChildTableEditorExit.Down;
                break;
            default:
                return false;
        }

        e.Handled = true;
        _exit(exit);
        return true;
    }
}
