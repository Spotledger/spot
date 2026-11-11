using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>The same control with editing off: a posted document, no write right, a read-only
/// table or cell. Nothing opens, nothing is claimed, nothing changes.</summary>
public sealed class ReadOnlyTests
{
    private static void Go(SpotChildTable table, int row, int column)
    {
        table.Grid.SetCursor(row, column);
        table.Grid.SetSelection([row]);
        table.Grid.Focus();
        TestHost.Pump();
    }

    public static TheoryData<string, bool> ReadOnlyCases => new()
    {
        { "no-right", false },
        { "posted", true },
        { "cancelled", true },
        { "unknown-state", true },
    };

    private static SpotChildTable ReadOnlyTable(string kind)
    {
        var state = kind switch { "posted" => "posted", "cancelled" => "cancelled", "unknown-state" => "frozen", _ => "draft" };
        return Keys.Show(Fixtures.Draft(4, state), canWrite: kind != "no-right");
    }

    [AvaloniaTheory]
    [MemberData(nameof(ReadOnlyCases))]
    public void NoEditorOpensAndNoEditingKeyChangesTheTable(string kind, bool _)
    {
        var table = ReadOnlyTable(kind);
        Assert.False(table.Model.IsEditable);
        Assert.Null(table.Grid.EditorFactory);
        var ids = table.Model.Ids();
        Go(table, 1, Fixtures.Qty);

        Keys.Press(table, Key.Enter);
        Keys.Press(table, Key.F2);
        Assert.False(table.Grid.IsEditing);
        Assert.False(table.Grid.BeginEdit(1, Fixtures.Qty));

        var keys = new (Key Key, KeyModifiers Modifiers)[]
        {
            (Key.I, KeyModifiers.Control), (Key.I, KeyModifiers.Control | KeyModifiers.Shift),
            (Key.Delete, KeyModifiers.Control), (Key.Up, KeyModifiers.Alt), (Key.D, KeyModifiers.Control),
            (Key.D, KeyModifiers.Control | KeyModifiers.Shift), (Key.V, KeyModifiers.Control),
            (Key.Z, KeyModifiers.Control), (Key.Y, KeyModifiers.Control),
        };
        table.Clipboard = new FakeClipboard("X\t9");
        foreach (var (key, modifiers) in keys)
        {
            Keys.Press(table, key, modifiers);
        }

        Assert.Equal(ids, table.Model.Ids());
        Assert.False(table.Model.Draft.IsDirty);
        Assert.Equal(0, table.Model.Table.JournalCount);
        Assert.False(table.Grid.IsEditing);
    }

    [AvaloniaTheory]
    [MemberData(nameof(ReadOnlyCases))]
    public void EveryPublicOperationOfTheControlIsRefusedWithAReason(string kind, bool _)
    {
        var table = ReadOnlyTable(kind);
        Go(table, 1, Fixtures.Qty);
        var results = new List<ChildTableResult>
        {
            table.AddRow(), table.InsertAbove(), table.InsertBelow(), table.DeleteSelected(), table.MoveSelected(1),
            table.DuplicateSelected(), table.FillDown(), table.Undo(), table.Redo(), table.PasteText("a\t1"),
        };
        Assert.All(results, result =>
        {
            Assert.False(result.Succeeded);
            Assert.False(string.IsNullOrEmpty(result.Message));
        });
        Assert.Equal(4, table.Model.RowCount);
        Assert.False(table.Model.Draft.IsDirty);
    }

    [AvaloniaFact]
    public async Task PasteFromTheClipboardIsRefusedToo()
    {
        var table = ReadOnlyTable("posted");
        table.Clipboard = new FakeClipboard("X\t9");
        var result = await table.PasteAsync();
        Assert.Equal(ChildTableRefusal.DocumentPosted, result.Refusal);
        Assert.False(table.Model.Draft.IsDirty);
    }

    [AvaloniaFact]
    public void ReadOnlyStillNavigatesSelectsAndShowsTheNoteOnAltDown()
    {
        var table = ReadOnlyTable("posted");
        ErrorMapper.Apply(table.Model.Draft, new Spotledger.Client.Api.ApiError
        {
            Code = "document.validation_failed",
            Message = "x",
            FieldErrors = [new Spotledger.Client.Api.ApiFieldError("items[2].qty", "c", "Looks wrong")],
        });
        Go(table, 1, Fixtures.Qty);
        // Alt+Down is the grid's "show this cell's note" key; a read-only table leaves it alone.
        Keys.Press(table, Key.Down, KeyModifiers.Alt);
        Assert.Equal(["r1", "r2", "r3", "r4"], table.Model.Ids());
        Assert.Equal(1, table.Grid.GridCellCursor.RowIndex);
        Keys.Press(table, Key.Down);
        Assert.Equal(2, table.Grid.GridCellCursor.RowIndex);
        Assert.Equal(new GridCursor(1, Fixtures.Qty, GridCursorOrigin.Keyboard), table.Grid.FindNextAnnotation());
    }

    [AvaloniaFact]
    public void ReadOnlyCellOpensNoEditorWhileItsNeighbourDoes()
    {
        var table = Keys.Show(Fixtures.Draft(3));
        Go(table, 1, Fixtures.Amount);
        Keys.Press(table, Key.F2);
        Assert.False(table.Grid.IsEditing);
        Go(table, 1, Fixtures.Rate);
        Keys.Press(table, Key.F2);
        Assert.True(table.Grid.IsEditing);
    }

    [AvaloniaFact]
    public void MaskedCellOpensNoEditor()
    {
        var permissions = Fixtures.Permissions(line: """{"masked":["rate"]}""");
        var table = Keys.Show(Fixtures.Draft(3, permissions: permissions));
        Go(table, 1, Fixtures.Rate);
        Keys.Press(table, Key.F2);
        Assert.False(table.Grid.IsEditing);
        Assert.False(table.Grid.BeginEdit(1, Fixtures.Rate));
    }

    [AvaloniaFact]
    public void RevokingTheRightWhileACellIsOpenClosesItWithoutCommitting()
    {
        var table = Keys.Show(Fixtures.Draft(3));
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.F2);
        Keys.Type(table, "9");
        Assert.True(table.Grid.IsEditing);

        table.CanWrite = false;
        Assert.False(table.Grid.IsEditing);
        Assert.Null(table.Grid.EditorFactory);
        Assert.Equal("2.00", table.Model.Text(1, Fixtures.Qty));
        Assert.False(table.Model.Draft.IsDirty);
        Assert.Null(table.CurrentHost);
    }

    [AvaloniaFact]
    public void ADocumentThatBecomesPostedWhileACellIsOpenClosesItWithoutCommitting()
    {
        var table = Keys.Show(Fixtures.Draft(3));
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.F2);
        Keys.Type(table, "9");

        table.Model.Draft.LoadFrom("""{"_state":"posted"}""");
        Assert.False(table.Grid.IsEditing);
        Assert.Equal("2.00", table.Model.Text(1, Fixtures.Qty));
        Assert.False(table.Model.Draft.IsDirty);
    }

    [AvaloniaFact]
    public void ACommitIsCheckedAgainAndRefusedWhenTheRightIsGoneBeforeIt()
    {
        var model = Fixtures.Model(Fixtures.Draft(3));
        var factory = new ChildTableEditor(model);
        var host = Assert.IsType<CellEditorHost>(factory.CreateEditor(1, Fixtures.Qty, null!, null!));
        host.Scratch.Value = new DecimalFieldValue(9m);
        model.CanWrite = false;

        Assert.False(factory.TryCommit(1, Fixtures.Qty, "9.00", host));
        Assert.NotNull(host.ErrorText);
        Assert.Equal("2.00", model.Text(1, Fixtures.Qty));
        Assert.False(model.Draft.IsDirty);
        Assert.Null(factory.CreateEditor(1, Fixtures.Qty, null!, null!));
    }

    [AvaloniaFact]
    public void ACommitWritesToTheRowItWasOpenedOnEvenIfRowsMoved()
    {
        var model = Fixtures.Model(Fixtures.Draft(3));
        var factory = new ChildTableEditor(model);
        var host = Assert.IsType<CellEditorHost>(factory.CreateEditor(2, Fixtures.Qty, null!, null!));
        host.Scratch.Value = new DecimalFieldValue(9m);
        model.MoveRows([2], -1);
        Assert.True(factory.TryCommit(2, Fixtures.Qty, "9.00", host));
        Assert.Equal("9.00", model.Text(1, Fixtures.Qty));
        Assert.Equal("2.00", model.Text(2, Fixtures.Qty));
    }

    [AvaloniaFact]
    public void ACommitIsRefusedWhenItsRowIsGone()
    {
        var model = Fixtures.Model(Fixtures.Draft(3));
        var factory = new ChildTableEditor(model);
        var host = Assert.IsType<CellEditorHost>(factory.CreateEditor(1, Fixtures.Qty, null!, null!));
        host.Scratch.Value = new DecimalFieldValue(9m);
        model.DeleteRows([1]);
        Assert.False(factory.TryCommit(1, Fixtures.Qty, "9.00", host));
        Assert.Equal(["r1", "r3"], model.Ids());
    }

    [AvaloniaFact]
    public void TheDraftAndTheControlAgreeOnWhoCanWrite()
    {
        var table = Keys.Show(Fixtures.Draft(2), canWrite: false);
        Assert.False(table.CanWrite);
        table.CanWrite = true;
        Assert.NotNull(table.Grid.EditorFactory);
        Go(table, 0, Fixtures.Qty);
        Keys.Press(table, Key.F2);
        Assert.True(table.Grid.IsEditing);
    }

    [AvaloniaFact]
    public void ALockedFieldKindIsShownButNotEdited()
    {
        var line = new DocTypeModel("line", "Line", 1, false,
        [
            new FieldModel("note", "Note", FieldKind.LongText),
            new FieldModel("tags", "Tags", FieldKind.Select, options: ["a", "b"], multiple: true),
            new FieldModel("name", "Name", FieldKind.Text),
        ]);
        var model = new DocTypeModel("doc", "Doc", 1, false,
            [new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line")],
            new Dictionary<string, DocTypeModel> { ["items"] = line });
        var draft = new DocumentDraft(model);
        draft.LoadFrom("""{"items":[{"id":"a","note":"long","tags":["a"],"name":"n"}]}""");
        var table = new SpotChildTable(draft, "items") { CanWrite = true };
        TestHost.Show(table);
        Go(table, 0, 0);
        Keys.Press(table, Key.F2);
        Assert.False(table.Grid.IsEditing);
        Go(table, 0, 1);
        Keys.Press(table, Key.F2);
        Assert.False(table.Grid.IsEditing);
        Go(table, 0, 2);
        Keys.Press(table, Key.F2);
        Assert.True(table.Grid.IsEditing);
        Assert.IsType<SpotTextInput>(table.CurrentHost!.Editor);
    }
}
