using Avalonia.Headless.XUnit;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>Selection semantics: single, extended (ctrl/shift), range, and the
/// survival of checked/selected state across a source reset (keyed by row key).</summary>
public sealed class SelectionTests
{
    private static SpotGrid MakeGrid(out InMemoryGridSource source, GridSelectionMode mode = GridSelectionMode.Extended)
    {
        source = new InMemoryGridSource(GridFixture.MakeRows(100, 4));
        return GridFixtures.Make(source, mode)
            .With(source, GridFixtures.PixelColumns(4, 90))
            .Build();
    }

    [AvaloniaFact]
    public void ExtendedClickReplacesThenCtrlToggles()
    {
        var grid = MakeGrid(out _);
        grid.Selection.Apply(2, GridSelectionModel.ExtendMode.Replace);
        Assert.Equal([2], grid.Selection.SelectedRows);

        grid.Selection.Apply(5, GridSelectionModel.ExtendMode.Toggle);
        Assert.Equal(2, grid.Selection.SelectedCount);

        grid.Selection.Apply(5, GridSelectionModel.ExtendMode.Toggle);
        Assert.Equal([2], grid.Selection.SelectedRows);
    }

    [AvaloniaFact]
    public void ShiftClickRangeSelectsContiguous()
    {
        var grid = MakeGrid(out _);
        grid.Selection.Apply(3, GridSelectionModel.ExtendMode.Replace);
        grid.Selection.Apply(8, GridSelectionModel.ExtendMode.Range);

        Assert.Equal(6, grid.Selection.SelectedCount);
        Assert.All(Enumerable.Range(3, 6), i => Assert.Contains(i, grid.Selection.SelectedRows));
    }

    [AvaloniaFact]
    public void SingleModeMovesTheSelection()
    {
        var grid = MakeGrid(out _, GridSelectionMode.OneRow);
        grid.Selection.Apply(1, GridSelectionModel.ExtendMode.Replace);
        grid.Selection.Apply(7, GridSelectionModel.ExtendMode.Toggle);

        Assert.Equal([7], grid.Selection.SelectedRows);
    }

    [AvaloniaFact]
    public void RangeModeClickThenShiftSelectsRange()
    {
        var grid = MakeGrid(out _, GridSelectionMode.Range);
        grid.Selection.Apply(2, GridSelectionModel.ExtendMode.Replace);
        grid.Selection.Apply(6, GridSelectionModel.ExtendMode.Range);
        Assert.Equal(5, grid.Selection.SelectedCount);
    }

    [AvaloniaFact]
    public void CheckStateIsIndependentOfSelection()
    {
        var grid = MakeGrid(out _);
        grid.Selection.Apply(4, GridSelectionModel.ExtendMode.Replace);
        grid.Selection.SetChecked(4, true);
        Assert.True(grid.Selection.HasFlag(4, GridRowState.Checked));
        Assert.True(grid.Selection.HasFlag(4, GridRowState.Selected));

        grid.Selection.ClearSelection();
        Assert.False(grid.Selection.HasFlag(4, GridRowState.Selected));
        Assert.True(grid.Selection.HasFlag(4, GridRowState.Checked), "the check must survive a deselect");
    }

    [AvaloniaFact]
    public async Task SelectionSurvivesSortBecauseItIsKeyed()
    {
        var grid = MakeGrid(out var source);
        grid.Selection.Apply(1, GridSelectionModel.ExtendMode.Replace); // r1
        Assert.Equal("r1", grid.Selection.Flags.Keys.Single());

        await source.Sort(0, GridSortDirection.Ascending);
        TestHost.Pump();

        Assert.Equal("r1", grid.Selection.Flags.Keys.Single());
    }

    [AvaloniaFact]
    public void ResetClearsSelectionWhenIdentitiesDied()
    {
        var grid = MakeGrid(out var source);
        grid.Selection.Apply(3, GridSelectionModel.ExtendMode.Replace);
        source.AddRow(GridRow.Of("other", new GridCell("x")));
        TestHost.Pump();
        // Reset raised → selection cleared (new identity space).
        Assert.Empty(grid.Selection.SelectedRows);
    }
}