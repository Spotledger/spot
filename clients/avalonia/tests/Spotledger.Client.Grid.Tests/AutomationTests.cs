using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>Accessibility: the grid peer exposes the whole-set counts, row peers exist
/// only for the visible window, and the selection pattern works through the peers.</summary>
public sealed class AutomationTests
{
    private static SpotGrid MakeGrid(int rows = 1000)
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(rows, 2));
        var grid = GridFixtures.Make(source)
            .With(source, new List<GridColumnModel>
            {
                new() { Header = "A", Width = GridColumnWidth.Pixels(100) },
                new() { Header = "B", Width = GridColumnWidth.Star(1) },
            })
            .Build();
        grid.SetViewport(400, 200);
        grid.ComputeColumnLayout(400);
        return grid;
    }

    [AvaloniaFact]
    public void GridPeerIsADataGridPeerWithSelectionPattern()
    {
        var grid = MakeGrid();
        var peer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
        Assert.Equal(AutomationControlType.DataGrid, peer.GetAutomationControlType());
        Assert.Equal("Spotledger data grid", peer.GetName());
        Assert.True(Assert.IsType<ISelectionProvider>(peer, exactMatch: false) is not null);
    }

    [AvaloniaFact]
    public void RowPeersCoverOnlyTheVisibleWindow()
    {
        var grid = MakeGrid();
        var peer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
        var children = peer.GetChildren() ?? [];

        Assert.Equal(grid.VisibleRowCount(), children.Count);
        Assert.True(children.All(c => c is SpotGridRowAutomationPeer));
    }

    [AvaloniaFact]
    public void RowPeerNamesCarryTheWholeSetCounts()
    {
        var grid = MakeGrid();
        var peer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
        var first = Assert.IsType<SpotGridRowAutomationPeer>(peer.GetChildren()![0]);
        Assert.Equal("row 1 of 1000", first.GetName());
    }

    [AvaloniaFact]
    public void CellPeersNameColumnHeaderPlusText()
    {
        var grid = MakeGrid(rows: 5);
        var gridPeer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
        var row = Assert.IsType<SpotGridRowAutomationPeer>(gridPeer.GetChildren()![0]);
        var cells = Assert.IsAssignableFrom<IReadOnlyList<AutomationPeer>>(row.GetChildren());
        Assert.Equal(2, cells.Count);
        Assert.Equal("A: row 0", cells[0].GetName());
    }

    [AvaloniaFact]
    public void SelectionThroughThePeerSelectsTheRow()
    {
        var grid = MakeGrid(rows: 20);
        var gridPeer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
        var row = Assert.IsType<SpotGridRowAutomationPeer>(gridPeer.GetChildren()![3]);
        row.Select();

        Assert.Equal([3], grid.Selection.SelectedRows);
        var selection = Assert.IsAssignableFrom<ISelectionProvider>(gridPeer);
        Assert.Single(selection.GetSelection());
    }

    [AvaloniaFact]
    public void WholeSetModeExposesEveryRowForSmallFixtures()
    {
        SpotGrid.AutomationForWholeSet = true;
        try
        {
            var grid = MakeGrid(rows: 12);
            grid.SetViewport(400, 120);

            var peer = Assert.IsType<SpotGridAutomationPeer>(grid.OnCreateAutomationPeerShim());
            Assert.Equal(12, peer.GetChildren()!.Count);
        }
        finally
        {
            SpotGrid.AutomationForWholeSet = false;
        }
    }
}