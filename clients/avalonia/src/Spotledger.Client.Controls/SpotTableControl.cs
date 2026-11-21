using Avalonia;
using Avalonia.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>A generic child-row editor. Row metadata comes from TableFieldValue, not guessed fields.
/// Large virtualized lists are provided by the separate Grid library, which depends on Controls.</summary>
public sealed class SpotTableControl : CatalogueControl
{
    public static readonly StyledProperty<TableFieldValue> ValueProperty = AvaloniaProperty.Register<SpotTableControl, TableFieldValue>(nameof(Value), new TableFieldValue([], null));
    public TableFieldValue Value { get => GetValue(ValueProperty); set => SetValue(ValueProperty, value); }
    private readonly List<(TableRowValue Row, Dictionary<string, FieldState> States, List<(Control Editor, FieldState State)> Editors)> _rows = [];
    private bool _committing;
    public SpotTableControl() : base("Child rows") => UpdatePresentation();
    public bool TryCommit()
    {
        if (IsReadOnly || Value.RowModel is null) return true;
        var valid = true;
        foreach (var row in _rows)
            foreach (var (editor, state) in row.Editors)
                valid &= FieldControlFactory.GetBinding(editor).CommitTo(state);
        if (!valid) return false;
        _committing = true;
        try { SetCurrentValue(ValueProperty, new TableFieldValue(_rows.Select(row => new TableRowValue(row.Row.Id, row.States.ToDictionary(x => x.Key, x => x.Value.Value))).ToArray(), Value.RowModel)); }
        finally { _committing = false; }
        return true;
    }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == ValueProperty && !_committing) UpdatePresentation();
    }
    protected override void UpdatePresentation()
    {
        if (Body is null) return;
        Body.Children.Clear();
        _rows.Clear();
        if (IsReadOnly || Value.RowModel is null)
        {
            var display = Display("Rows");
            display.Text = Value.Rows.Count == 0 ? "No rows" : FieldControlFactory.FormatValue(Value);
            display.AcceptsReturn = true;
            Body.Children.Add(display);
            return;
        }
        for (var index = 0; index < Value.Rows.Count; index++)
        {
            var rowIndex = index;
            var row = Value.Rows[index];
            var states = row.Values.ToDictionary(x => x.Key, x => new FieldState(x.Value));
            var editors = new List<(Control, FieldState)>();
            var panel = new StackPanel();
            foreach (var field in Value.RowModel.Fields)
            {
                if (!states.TryGetValue(field.Name, out var state)) { state = new FieldState(field.Default ?? field.Kind.EmptyValue()); states.Add(field.Name, state); }
                var editor = FieldControlFactory.Create(field, state);
                editors.Add((editor, state));
                panel.Children.Add(new SpotField { Label = field.Label, IsRequired = field.Required, Content = editor });
            }
            panel.Children.Add(Action("Remove row", () => { if (TryCommit()) Value = new TableFieldValue(Value.Rows.Where((_, i) => i != rowIndex).ToArray(), Value.RowModel); }));
            _rows.Add((row, states, editors));
            Body.Children.Add(panel);
        }
        Body.Children.Add(Action("Add row", () =>
        {
            if (!TryCommit()) return;
            Value = new TableFieldValue(Value.Rows.Append(TableRowValue.New(new Dictionary<string, FieldValue>())).ToArray(), Value.RowModel);
        }));
    }
}
