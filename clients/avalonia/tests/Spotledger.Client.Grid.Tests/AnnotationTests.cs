using Avalonia;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Platform;
using Avalonia.Styling;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>Cell and row annotations (WP-UI-08.T5): the record and its bound, drawing
/// (confined to the marker corner, shape per severity, nothing when absent), hit-testing,
/// tooltip, the Alt+Down key, <see cref="SpotGrid.FindNextAnnotation"/> and the
/// accessibility text.</summary>
public sealed class AnnotationTests
{
    private static readonly GridCellAnnotation Err = new(GridAnnotationSeverity.Error, "Quantity is required");
    private static readonly GridCellAnnotation Warn = new(GridAnnotationSeverity.Warning, "Price is unusually low");
    private static readonly GridCellAnnotation Note = new(GridAnnotationSeverity.Info, "Defaulted from the item");

    /// <summary>Rows of three cells; <paramref name="notes"/> maps (row, column) to a cell annotation.</summary>
    private static List<GridRow> Rows(
        int count,
        Dictionary<(int Row, int Column), GridCellAnnotation>? notes = null,
        Dictionary<int, GridCellAnnotation>? rowNotes = null)
    {
        var rows = new List<GridRow>(count);
        for (var r = 0; r < count; r++)
        {
            var cells = new GridCell[3];
            for (var c = 0; c < 3; c++)
            {
                cells[c] = new GridCell($"r{r}c{c}")
                {
                    Annotation = notes is not null && notes.TryGetValue((r, c), out var n) ? n : null,
                };
            }

            rows.Add(new GridRow($"k{r}", cells)
            {
                RowAnnotation = rowNotes is not null && rowNotes.TryGetValue(r, out var rn) ? rn : null,
            });
        }

        return rows;
    }

    private static SpotGrid MakeGrid(List<GridRow> rows, double width = 420, double height = 300)
    {
        var source = new InMemoryGridSource(rows);
        var grid = GridFixtures.Make(source)
            .With(source, GridFixtures.PixelColumns(3, 120), width: width, height: height)
            .Build();
        TestHost.Pump();
        return grid;
    }

    private static Point CenterOf(SpotGrid grid, int row, int column)
        => new(grid.ColumnX(column) + 20, grid.RowY(row) + (grid.RowHeight / 2));

    private static void Press(SpotGrid grid, Key key, KeyModifiers modifiers = KeyModifiers.None)
        => grid.RaiseEvent(new KeyEventArgs
        {
            Key = key,
            KeyModifiers = modifiers,
            RoutedEvent = InputElement.KeyDownEvent,
        });

    // ---- the record -------------------------------------------------------------

    [Fact]
    public void ShortMessageIsKeptAsIs()
    {
        var note = new GridCellAnnotation(GridAnnotationSeverity.Error, new string('a', 512));
        Assert.Equal(512, note.Message.Length);
        Assert.Equal(new string('a', 512), note.Message);
    }

    [Fact]
    public void LongMessageIsCutToFiveHundredTwelveWithAnEllipsis()
    {
        var note = new GridCellAnnotation(GridAnnotationSeverity.Warning, new string('a', 600));
        Assert.Equal(512, note.Message.Length);
        Assert.EndsWith("…", note.Message);
        Assert.Equal(new string('a', 511), note.Message[..511]);
    }

    [Fact]
    public void CutNeverSplitsASurrogatePair()
    {
        // A pair straddles the cut: 510 letters, then a pair at 510..511, then more text.
        var text = new string('a', 510) + "\U0001F600" + new string('b', 20);
        var note = new GridCellAnnotation(GridAnnotationSeverity.Info, text);
        Assert.True(note.Message.Length <= 512);
        Assert.EndsWith("…", note.Message);
        Assert.False(char.IsHighSurrogate(note.Message[^2]));
    }

    [Fact]
    public void InitSetterAlsoBoundsTheMessage()
    {
        var note = new GridCellAnnotation(GridAnnotationSeverity.Info, "x") { Message = new string('z', 900) };
        Assert.Equal(512, note.Message.Length);
    }

    [Fact]
    public void NullMessageBecomesEmpty()
    {
        Assert.Equal(string.Empty, new GridCellAnnotation(GridAnnotationSeverity.Info, null!).Message);
    }

    [Fact]
    public void AnnotationsHaveValueEquality()
    {
        Assert.Equal(Err, new GridCellAnnotation(GridAnnotationSeverity.Error, "Quantity is required"));
        Assert.NotEqual(Err, new GridCellAnnotation(GridAnnotationSeverity.Warning, "Quantity is required"));
    }

    [Fact]
    public void CellAndRowDefaultToNoAnnotationAndEqualityIsUnchanged()
    {
        var plain = new GridCell("a");
        Assert.Null(plain.Annotation);
        Assert.Equal(new GridCell("a", GridCellAlignment.Start, null), plain);
        Assert.NotEqual(plain, plain with { Annotation = Err });
        Assert.Null(GridRow.Of("k", plain).RowAnnotation);
    }

    // ---- hit-testing ----------------------------------------------------------------

    [AvaloniaFact]
    public void HitTestReturnsTheCellAnnotationOnlyUnderThatCell()
    {
        var grid = MakeGrid(Rows(10, new() { [(2, 1)] = Err }));
        Assert.Same(Err, grid.GetAnnotationAt(CenterOf(grid, 2, 1)));
        Assert.Null(grid.GetAnnotationAt(CenterOf(grid, 2, 0)));
        Assert.Null(grid.GetAnnotationAt(CenterOf(grid, 3, 1)));
        Assert.Null(grid.GetAnnotationAt(new Point(-5, -5)));
    }

    [AvaloniaFact]
    public void RowAnnotationAnswersInTheFirstColumnAndACellNoteWins()
    {
        var grid = MakeGrid(Rows(10, new() { [(4, 0)] = Warn }, new() { [3] = Err, [4] = Note }));
        Assert.Same(Err, grid.GetAnnotationAt(CenterOf(grid, 3, 0)));
        Assert.Null(grid.GetAnnotationAt(CenterOf(grid, 3, 1)));
        Assert.Same(Warn, grid.GetAnnotationAt(CenterOf(grid, 4, 0)));
    }

    // ---- tooltip --------------------------------------------------------------------

    [AvaloniaFact]
    public void HoverShowsTheMessageAndLeavingClearsIt()
    {
        var grid = MakeGrid(Rows(10, new() { [(2, 1)] = Err }));
        grid.UpdateAnnotationTooltip(CenterOf(grid, 2, 1));
        Assert.Equal("Quantity is required", grid.AnnotationTooltipText);

        grid.UpdateAnnotationTooltip(CenterOf(grid, 2, 2));
        Assert.Null(grid.AnnotationTooltipText);

        grid.UpdateAnnotationTooltip(CenterOf(grid, 2, 1));
        grid.ClearAnnotationTooltip();
        Assert.Null(grid.AnnotationTooltipText);
    }

    [AvaloniaFact]
    public void AltDownOpensTheTooltipForTheCurrentCellWithoutMovingTheCursor()
    {
        var grid = MakeGrid(Rows(10, new() { [(1, 1)] = Err }));
        grid.CellCursor = new GridCursor(1, 1, GridCursorOrigin.Keyboard);

        Press(grid, Key.Down, KeyModifiers.Alt);

        Assert.Equal("Quantity is required", grid.AnnotationTooltipText);
        Assert.True(grid.KeyboardTooltipOpen);
        Assert.Equal(1, grid.CellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void AltDownOnAPlainCellStillMovesDown()
    {
        var grid = MakeGrid(Rows(10, new() { [(1, 1)] = Err }));
        grid.CellCursor = new GridCursor(0, 1, GridCursorOrigin.Keyboard);

        Press(grid, Key.Down, KeyModifiers.Alt);

        Assert.False(grid.KeyboardTooltipOpen);
        Assert.Equal(1, grid.CellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void EscapeAndOtherKeysDismissTheKeyboardTooltip()
    {
        var grid = MakeGrid(Rows(10, new() { [(1, 1)] = Err }));
        grid.CellCursor = new GridCursor(1, 1, GridCursorOrigin.Keyboard);

        Press(grid, Key.Down, KeyModifiers.Alt);
        Press(grid, Key.Escape);
        Assert.False(grid.KeyboardTooltipOpen);
        Assert.Equal(1, grid.CellCursor.RowIndex);

        Press(grid, Key.Down, KeyModifiers.Alt);
        Press(grid, Key.Right);
        Assert.False(grid.KeyboardTooltipOpen);
        Assert.Equal(2, grid.CellCursor.ColumnIndex);
    }

    // ---- next annotation --------------------------------------------------------------

    [AvaloniaFact]
    public void FindNextWalksReadingOrderAndWraps()
    {
        var grid = MakeGrid(Rows(10, new() { [(1, 2)] = Err, [(4, 0)] = Warn, [(4, 2)] = Err, [(8, 1)] = Err }));

        var first = grid.FindNextAnnotation(null)!.Value;
        Assert.Equal((1, 2), (first.RowIndex, first.ColumnIndex));

        var second = grid.FindNextAnnotation(first)!.Value;
        Assert.Equal((4, 0), (second.RowIndex, second.ColumnIndex));

        var third = grid.FindNextAnnotation(second)!.Value;
        Assert.Equal((4, 2), (third.RowIndex, third.ColumnIndex));

        var fourth = grid.FindNextAnnotation(third)!.Value;
        Assert.Equal((8, 1), (fourth.RowIndex, fourth.ColumnIndex));

        var wrapped = grid.FindNextAnnotation(fourth)!.Value;
        Assert.Equal((1, 2), (wrapped.RowIndex, wrapped.ColumnIndex));
    }

    [AvaloniaFact]
    public void FindNextFiltersBySeverity()
    {
        var grid = MakeGrid(Rows(10, new() { [(1, 2)] = Err, [(4, 0)] = Warn, [(8, 1)] = Err }));

        var next = grid.FindNextAnnotation(new GridCursor(1, 2), GridAnnotationSeverity.Error)!.Value;
        Assert.Equal((8, 1), (next.RowIndex, next.ColumnIndex));

        var warning = grid.FindNextAnnotation(new GridCursor(8, 1), GridAnnotationSeverity.Warning)!.Value;
        Assert.Equal((4, 0), (warning.RowIndex, warning.ColumnIndex));

        Assert.Null(grid.FindNextAnnotation(null, GridAnnotationSeverity.Info));
    }

    [AvaloniaFact]
    public void FindNextOnASingleAnnotationFindsItselfAfterALap()
    {
        var grid = MakeGrid(Rows(10, new() { [(5, 1)] = Err }));
        var next = grid.FindNextAnnotation(new GridCursor(5, 1))!.Value;
        Assert.Equal((5, 1), (next.RowIndex, next.ColumnIndex));
    }

    [AvaloniaFact]
    public void FindNextWithNoAnnotationsHasNoValue()
    {
        var grid = MakeGrid(Rows(10));
        Assert.Null(grid.FindNextAnnotation(null));
    }

    [AvaloniaFact]
    public void FindNextCountsRowAnnotationsAtTheFirstColumn()
    {
        var grid = MakeGrid(Rows(10, null, new() { [6] = Warn }));
        var next = grid.FindNextAnnotation(null)!.Value;
        Assert.Equal((6, 0), (next.RowIndex, next.ColumnIndex));
    }

    // ---- accessibility ----------------------------------------------------------------

    [AvaloniaFact]
    public void CellPeerExposesTheAnnotationAsHelpText()
    {
        var grid = MakeGrid(Rows(5, new() { [(1, 1)] = Err }));
        var gridPeer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
        var row = Assert.IsType<SpotGridRowAutomationPeer>(gridPeer.GetChildren()![1]);
        var cells = row.GetChildren()!;

        Assert.Equal("Error: Quantity is required", cells[1].GetHelpText());
        Assert.Equal(string.Empty, cells[0].GetHelpText());
        Assert.Equal("C1: r1c1", cells[1].GetName());
    }

    [AvaloniaFact]
    public void RowPeerExposesTheRowAnnotation()
    {
        var grid = MakeGrid(Rows(5, null, new() { [2] = Warn }));
        var gridPeer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
        var row = Assert.IsType<SpotGridRowAutomationPeer>(gridPeer.GetChildren()![2]);
        Assert.Equal("Warning: Price is unusually low", row.GetHelpText());
    }

    [AvaloniaFact]
    public void CurrentCellTextFollowsTheCursor()
    {
        var grid = MakeGrid(Rows(5, new() { [(1, 1)] = Err }));
        Assert.Null(grid.CurrentCellAnnotationText());
        grid.CellCursor = new GridCursor(1, 1, GridCursorOrigin.Keyboard);
        Assert.Equal("Quantity is required", grid.CurrentCellAnnotationText());
        grid.CellCursor = new GridCursor(1, 2, GridCursorOrigin.Keyboard);
        Assert.Null(grid.CurrentCellAnnotationText());
    }
}
