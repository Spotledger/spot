using Spotledger.Client.Grid;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Grid.Gallery;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>Gallery sanity: the sample page builds and behaves (a real integration test
/// through the public gallery entry point).</summary>
public sealed class GridGalleryTests
{
    [AvaloniaFact]
    public void SamplePageBuildsAndFetchesItsFirstWindow()
    {
        var grid = GridGalleryPage.BuildSample();
        TestHost.Show(grid, 900, 600);
        TestHost.Pump();
        TestHost.Pump();

        Assert.Equal(1000, grid.TotalRows);
        Assert.NotNull(grid.AppliedRows);
        Assert.Equal(6, grid.ColumnCount);
        Assert.NotNull(grid.Footer);
        Assert.True(grid.Columns!.Any(c => c.IsCheckColumn), "the sample shows the check column");
    }

    [AvaloniaFact]
    public void SampleSortPushesToTheSource()
    {
        var grid = GridGalleryPage.BuildSample();
        TestHost.Show(grid, 900, 600);
        TestHost.Pump();

        grid.CycleSort(0);
        Assert.Equal((0, GridSortDirection.Ascending), grid.SortState);
    }
}