using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists;

public sealed partial class ListView
{
    private readonly SpotSelect _fieldBox = new() { MinWidth = 180 };
    private readonly SpotSelect _operatorBox = new() { MinWidth = 150 };
    private readonly SpotTextInput _first = new() { MinWidth = 160 };
    private readonly SpotTextInput _second = new() { MinWidth = 160, Placeholder = "to" };
    private readonly SpotSelect _flag = new() { MinWidth = 100 };
    private readonly WrapPanel _choices = new() { Orientation = Orientation.Horizontal };
    private readonly SpotMultiSelect _links = new() { MinWidth = 220 };
    private readonly TextBlock _builderError = new() { IsVisible = false };
    private Border? _builderPanel;
    private IReadOnlyList<FieldModel> _builderFields = [];

    private Border BuildBuilder()
    {
        var add = new SpotButton { Content = "Apply filter" };
        var cancel = new SpotButton { Content = "Cancel", Variant = SpotButtonVariant.Subtle };
        AutomationProperties.SetName(_fieldBox, "Filter field");
        AutomationProperties.SetName(_operatorBox, "Filter operator");
        AutomationProperties.SetName(_first, "Filter value");
        AutomationProperties.SetName(_second, "Filter upper value");
        var row = new WrapPanel { Orientation = Orientation.Horizontal };
        foreach (var control in new Control[] { _fieldBox, _operatorBox, _first, _second, _flag, _choices, _links, add, cancel })
        {
            control.Margin = new Thickness(0, 0, Metric("Space.2", 8), Metric("Space.2", 8));
            row.Children.Add(control);
        }

        var panel = new StackPanel { Spacing = Metric("Space.1", 4) };
        panel.Children.Add(row);
        panel.Children.Add(_builderError);
        _builderPanel = new Border { IsVisible = false, Padding = new Thickness(Metric("Space.2", 8)), Child = panel };
        _addFilter.Click += (_, _) => OpenBuilder();
        cancel.Click += (_, _) => _builderPanel.IsVisible = false;
        add.Click += (_, _) => ApplyBuilder();
        _fieldBox.SelectionChanged += (_, _) => OnBuilderFieldChanged();
        _operatorBox.SelectionChanged += (_, _) => OnBuilderOperatorChanged();
        return _builderPanel;
    }

    /// <summary>Opens the "add filter" builder (the standard filters of the view come first).</summary>
    public void OpenBuilder()
    {
        _builderFields = _model.FilterFields;
        if (_builderFields.Count == 0 || _builderPanel is null)
        {
            return;
        }

        _fieldBox.Options = _builderFields.Select(f => new SelectOption(f.Name, f.Label)).ToArray();
        _fieldBox.SelectedIndex = 0;
        _builderError.IsVisible = false;
        _builderPanel.IsVisible = true;
        OnBuilderFieldChanged();
        _fieldBox.Focus();
    }

    private FieldModel? BuilderField => _fieldBox.SelectedItem is SelectOption { Value: string name } ? _builderFields.FirstOrDefault(f => f.Name == name) : null;

    private FilterOperator? BuilderOperator => _operatorBox.SelectedItem is SelectOption { Value: FilterOperator op } ? op : null;

    private static string OperatorLabel(FilterOperator op) => op switch
    {
        FilterOperator.Contains => "contains",
        FilterOperator.Equals => "is",
        FilterOperator.NotEquals => "is not",
        FilterOperator.StartsWith => "starts with",
        FilterOperator.From => "from",
        FilterOperator.To => "up to",
        FilterOperator.Between => "between",
        FilterOperator.OneOf => "is one of",
        FilterOperator.Is => "is",
        FilterOperator.IsEmpty => "is empty",
        _ => "has a value",
    };

    private void OnBuilderFieldChanged()
    {
        if (BuilderField is not { } field)
        {
            return;
        }

        _operatorBox.Options = FilterCondition.OperatorsFor(field.Kind).Select(o => new SelectOption(o, OperatorLabel(o))).ToArray();
        _operatorBox.SelectedIndex = 0;
        _choices.Children.Clear();
        if (field.Kind == FieldKind.Select)
        {
            foreach (var option in field.Options.Take(40))
            {
                _choices.Children.Add(new SpotCheckBox { Content = option, Tag = option, Margin = new Thickness(0, 0, 8, 0) });
            }
        }

        _flag.Options = [new SelectOption("true", "set"), new SelectOption("false", "not set")];
        _flag.SelectedIndex = 0;
        _links.Source = field.Kind == FieldKind.Link ? _lookups?.Invoke(_model.DocType, field.Name) : null;
        _links.SelectedItems = [];
        OnBuilderOperatorChanged();
    }

    private void OnBuilderOperatorChanged()
    {
        var field = BuilderField;
        var op = BuilderOperator;
        if (field is null || op is null)
        {
            return;
        }

        var needs = FilterCondition.ValueCount(op.Value);
        var link = field.Kind == FieldKind.Link && _links.Source is not null && op == FilterOperator.OneOf;
        var select = field.Kind == FieldKind.Select && op == FilterOperator.OneOf;
        var flag = field.Kind == FieldKind.Bool;
        _first.IsVisible = needs != 0 && !link && !select && !flag;
        _second.IsVisible = needs == 2;
        _choices.IsVisible = select;
        _links.IsVisible = link;
        _flag.IsVisible = flag;
        _first.Placeholder = field.Kind == FieldKind.Link ? "Id (comma separated)" : op == FilterOperator.Between ? "from" : "value";
    }

    private void ApplyBuilder()
    {
        if (BuilderField is not { } field || BuilderOperator is not { } op)
        {
            return;
        }

        var values = new List<string>();
        if (field.Kind == FieldKind.Bool)
        {
            values.Add(_flag.SelectedItem is SelectOption { Value: string flag } ? flag : "true");
        }
        else if (op == FilterOperator.OneOf && field.Kind == FieldKind.Select)
        {
            values.AddRange(_choices.Children.OfType<SpotCheckBox>().Where(c => c.IsChecked == true).Select(c => (string)c.Tag!));
        }
        else if (op == FilterOperator.OneOf && _links.IsVisible)
        {
            values.AddRange(_links.SelectedItems.Select(i => i.Id));
        }
        else if (op == FilterOperator.OneOf)
        {
            values.AddRange((_first.Text ?? string.Empty).Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries));
        }
        else if (FilterCondition.ValueCount(op) >= 1)
        {
            values.Add(_first.Text ?? string.Empty);
            if (op == FilterOperator.Between)
            {
                values.Add(_second.Text ?? string.Empty);
            }
        }

        var condition = new FilterCondition(field.Name, op, values);
        if (condition.TryBuild(field, out _, out var error) && _model.AddCondition(condition))
        {
            _builderPanel!.IsVisible = false;
            _first.Text = _second.Text = string.Empty;
            return;
        }

        _builderError.Text = error ?? _model.Message;
        _builderError.IsVisible = true;
    }

    private void OnFiltersChanged(object? sender, EventArgs e)
    {
        _chips.Children.Clear();
        var index = 0;
        foreach (var condition in _model.Conditions)
        {
            var field = _model.FilterFields.FirstOrDefault(f => f.Name == condition.Field)
                ?? _model.ChooserFields.FirstOrDefault(f => f.Name == condition.Field);
            var at = index++;
            var chip = new SpotButton
            {
                Content = (field is null ? condition.Field : condition.Describe(field)) + "  ✕",
                Variant = SpotButtonVariant.Subtle,
                Margin = new Thickness(0, 0, Metric("Space.2", 8), Metric("Space.1", 4)),
            };
            AutomationProperties.SetName(chip, "Remove filter " + (field is null ? condition.Field : condition.Describe(field)));
            chip.Click += (_, _) => _model.RemoveCondition(at);
            chip.KeyDown += (_, args) =>
            {
                if (args.Key is Key.Delete or Key.Back)
                {
                    args.Handled = true;
                    _model.RemoveCondition(at);
                }
            };
            _chips.Children.Add(chip);
        }

        if (_chips.Children.Count > 0)
        {
            var clear = new SpotButton { Content = "Clear all", Variant = SpotButtonVariant.Subtle };
            clear.Click += (_, _) => { _search.Text = string.Empty; _model.ClearFilters(); };
            _chips.Children.Add(clear);
        }
    }

    private void ConfigureMenus()
    {
        _columns.Click += (_, _) => ShowColumnChooser();
        _saved.Click += (_, _) => ShowSavedMenu();
        var open = new MenuItem { Header = "Open" };
        var delete = new MenuItem { Header = "Delete selected" };
        var export = new MenuItem { Header = "Export CSV" };
        var refresh = new MenuItem { Header = "Refresh (F5)" };
        open.Click += (_, _) =>
        {
            if (_model.SelectedIds.Count > 0)
            {
                _model.Open(_model.SelectedIds[0]);
            }
        };
        delete.Click += async (_, _) => await _model.DeleteSelectedAsync().ConfigureAwait(true);
        export.Click += async (_, _) => await _model.ExportAsync().ConfigureAwait(true);
        refresh.Click += (_, _) => _model.Refresh();
        var menu = new ContextMenu { ItemsSource = new[] { open, delete, export, refresh } };
        menu.Opening += (_, _) =>
        {
            open.IsEnabled = _model.SelectedIds.Count > 0;
            delete.IsEnabled = _model.CanDelete;
            export.IsEnabled = _model.CanExport;
        };
        _grid.ContextMenu = menu;
    }

    private void ShowSavedMenu()
    {
        var items = new List<Control>();
        foreach (var saved in _model.SavedFilters)
        {
            var name = saved.Name;
            var apply = new MenuItem { Header = name };
            apply.Click += (_, _) =>
            {
                if (_model.ApplySavedFilter(name))
                {
                    _search.Text = _model.SearchText;
                }
            };
            items.Add(apply);
        }

        var save = new MenuItem { Header = "Save current filter…", IsEnabled = _model.Conditions.Count > 0 || _model.AppliedSearch.Length > 0 };
        save.Click += async (_, _) => await _model.PromptSaveFilterAsync().ConfigureAwait(true);
        if (items.Count > 0)
        {
            items.Add(new Separator());
            var remove = new MenuItem { Header = "Delete a saved filter" };
            remove.ItemsSource = _model.SavedFilters.Select(f =>
            {
                var item = new MenuItem { Header = f.Name };
                item.Click += async (_, _) => await _model.DeleteSavedFilterAsync(f.Name).ConfigureAwait(true);
                return item;
            }).ToArray();
            items.Add(remove);
        }

        items.Add(save);
        _saved.Flyout = new MenuFlyout { ItemsSource = items };
        _saved.Flyout.ShowAt(_saved);
    }

    private void ShowColumnChooser()
    {
        var content = new StackPanel { Spacing = Metric("Space.1", 4), MinWidth = 260 };
        var flyout = new Flyout { Content = content };

        void Rebuild()
        {
            content.Children.Clear();
            var visible = _model.CurrentLayout.Select(c => c.Field).ToList();
            var all = visible.Concat(_model.ChooserFields.Select(f => f.Name).Where(n => !visible.Contains(n))).ToList();
            foreach (var name in all)
            {
                var field = _model.ChooserFields.First(f => f.Name == name);
                var row = new DockPanel();
                var check = new SpotCheckBox { Content = field.Label, IsChecked = visible.Contains(name) };
                var up = new SpotButton { Content = "↑", Variant = SpotButtonVariant.Subtle, IsEnabled = visible.IndexOf(name) > 0 };
                var down = new SpotButton { Content = "↓", Variant = SpotButtonVariant.Subtle, IsEnabled = visible.IndexOf(name) is >= 0 && visible.IndexOf(name) < visible.Count - 1 };
                AutomationProperties.SetName(up, "Move " + field.Label + " earlier");
                AutomationProperties.SetName(down, "Move " + field.Label + " later");
                DockPanel.SetDock(down, Dock.Right);
                DockPanel.SetDock(up, Dock.Right);
                row.Children.Add(down);
                row.Children.Add(up);
                row.Children.Add(check);
                content.Children.Add(row);
                check.IsCheckedChanged += async (_, _) =>
                {
                    var next = visible.ToList();
                    if (check.IsChecked == true) next.Add(name); else if (next.Count > 1) next.Remove(name);
                    await Apply(next).ConfigureAwait(true);
                };
                up.Click += async (_, _) => await Apply(Swap(visible, visible.IndexOf(name), -1)).ConfigureAwait(true);
                down.Click += async (_, _) => await Apply(Swap(visible, visible.IndexOf(name), 1)).ConfigureAwait(true);
            }

            var reset = new SpotButton { Content = "Reset to default", Variant = SpotButtonVariant.Secondary };
            reset.Click += async (_, _) => { await _model.ResetColumnsAsync().ConfigureAwait(true); Rebuild(); };
            content.Children.Add(reset);
        }

        async Task Apply(List<string> names)
        {
            var widths = _model.CurrentLayout.ToDictionary(c => c.Field, c => c.Width);
            await _model.SetColumnsAsync(names.Select(n => new ColumnPreference(n, widths.GetValueOrDefault(n))).ToArray()).ConfigureAwait(true);
            Rebuild();
        }

        static List<string> Swap(List<string> list, int index, int delta)
        {
            var copy = list.ToList();
            var other = index + delta;
            if (index >= 0 && other >= 0 && other < copy.Count)
            {
                (copy[index], copy[other]) = (copy[other], copy[index]);
            }

            return copy;
        }

        Rebuild();
        _columns.Flyout = flyout;
        flyout.ShowAt(_columns);
    }
}
