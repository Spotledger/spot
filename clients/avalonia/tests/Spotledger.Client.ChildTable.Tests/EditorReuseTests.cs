using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>A column's editor is reused for every edit in it (building one costs more than a key may
/// take), so nothing of one edit may show in the next: not its text, not its message, not its list.</summary>
public sealed class EditorReuseTests
{
    private static void Go(SpotChildTable table, int row, int column)
    {
        table.Grid.SetCursor(row, column);
        table.Grid.SetSelection([row]);
        table.Grid.Focus();
        TestHost.Pump();
    }

    [AvaloniaFact]
    public void TheSameHostServesEveryEditInAColumn()
    {
        var table = Keys.Show(Fixtures.Draft(5));
        Go(table, 0, Fixtures.Qty);
        Keys.Press(table, Key.F2);
        var first = table.CurrentHost;
        Keys.Press(table, Key.Escape);
        Go(table, 3, Fixtures.Qty);
        Keys.Press(table, Key.F2);
        Assert.Same(first, table.CurrentHost);
        Assert.Equal(3, table.CurrentHost!.RowIndex);
        Go(table, 3, Fixtures.Rate);
        Keys.Press(table, Key.Escape);
        Keys.Press(table, Key.F2);
        Assert.NotSame(first, table.CurrentHost);
    }

    [AvaloniaFact]
    public void ACancelledEditLeavesNothingInTheNextOne()
    {
        var table = Keys.Show(Fixtures.Draft(5));
        Go(table, 0, Fixtures.Qty);
        Keys.Press(table, Key.F2);
        table.CurrentHost!.Editor.As<SpotDecimalInput>().Text = "7.777";
        Keys.Press(table, Key.Enter); // refused: too many decimals
        Assert.NotNull(table.EditorErrorText);
        Keys.Press(table, Key.Escape);

        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.F2);
        var box = table.CurrentHost!.Editor.As<SpotDecimalInput>();
        Assert.Null(table.EditorErrorText);
        Assert.Null(box.ErrorText);
        Assert.Equal("2.00", box.Text);
        Keys.Press(table, Key.Enter, KeyModifiers.Control);
        Assert.False(table.Model.Draft.IsDirty);
    }

    [AvaloniaFact]
    public async Task ALookupEditorStartsFromTheNextRowsValueWithAClosedList()
    {
        var lookup = new FakeLookup("ITEM-A", "ITEM-B");
        var table = Keys.Show(Fixtures.Draft(4), lookups: _ => lookup);
        Go(table, 0, Fixtures.Item);
        Keys.Press(table, Key.F2);
        var box = table.CurrentHost!.Editor.As<SpotLookupBox>();
        Keys.Type(table, "A");
        await box.PendingSearch;
        Assert.True(box.IsOpen);
        Keys.Press(table, Key.Escape); // closes the list
        Assert.True(table.Grid.IsEditing);
        Keys.Press(table, Key.Escape); // leaves the cell
        Assert.False(table.Grid.IsEditing);

        Go(table, 2, Fixtures.Item);
        Keys.Press(table, Key.F2);
        Assert.Same(box, table.CurrentHost!.Editor);
        Assert.False(box.IsOpen);
        Assert.Equal("ITEM-3", box.SelectedItem?.Id);
        Assert.Equal("ITEM-3", box.Editor.Text);
        Keys.Press(table, Key.Escape);
        Assert.False(table.Model.Draft.IsDirty);
    }

    [AvaloniaFact]
    public void EscapeWithNoListOpenLeavesTheCellOnEveryKindOfEditor()
    {
        var table = Keys.Show(Fixtures.Draft(3), lookups: _ => new FakeLookup("A"));
        foreach (var column in new[] { Fixtures.Item, Fixtures.Desc, Fixtures.Qty, Fixtures.Unit, Fixtures.Due, Fixtures.Taxable })
        {
            Go(table, 1, column);
            Keys.Press(table, Key.F2);
            Assert.True(table.Grid.IsEditing, $"column {column} opens");
            Keys.Press(table, Key.Escape);
            Assert.False(table.Grid.IsEditing, $"column {column} closes on Escape");
        }

        Assert.False(table.Model.Draft.IsDirty);
    }

    [AvaloniaFact]
    public void EveryKindOfEditorCommitsItsValueThroughTheDraft()
    {
        var table = Keys.Show(Fixtures.Draft(3));
        Go(table, 1, Fixtures.Due);
        Keys.Press(table, Key.F2);
        Assert.IsType<SpotDateInput>(table.CurrentHost!.Editor);
        Keys.Press(table, Key.A, KeyModifiers.Control);
        foreach (var ch in "29-02-2028")
        {
            Keys.Type(table, ch.ToString());
        }

        Keys.Press(table, Key.Enter, KeyModifiers.Control);
        Assert.Equal("2028-02-29", table.Model.Text(1, Fixtures.Due));

        Go(table, 1, Fixtures.Taxable);
        Keys.Press(table, Key.F2);
        var check = table.CurrentHost!.Editor.As<SpotCheckBox>();
        check.IsChecked = false;
        Keys.Press(table, Key.Enter, KeyModifiers.Control);
        Assert.Equal("false", table.Model.Text(1, Fixtures.Taxable));

        Go(table, 1, Fixtures.Unit);
        Keys.Press(table, Key.F2);
        var select = table.CurrentHost!.Editor.As<SpotSelect>();
        select.SelectedItem = select.Options!.First(option => (string?)option.Value == "Box");
        Keys.Press(table, Key.Enter, KeyModifiers.Control);
        Assert.Equal("Box", table.Model.Text(1, Fixtures.Unit));

        Assert.Equal(["items[2].due", "items[2].taxable", "items[2].unit"], table.Model.Draft.DirtyPaths().Order());
        Assert.Equal(3, table.Model.Table.JournalCount);
    }
}

internal static class ControlExtensions
{
    public static T As<T>(this Control control) where T : Control => Assert.IsType<T>(control);
}
