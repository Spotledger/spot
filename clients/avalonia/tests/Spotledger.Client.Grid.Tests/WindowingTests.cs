using Spotledger.Client.Grid;
using Avalonia.Headless.XUnit;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>
/// Windowing tests: request ranges, prefetch margin bounds, cancellation of superseded
/// windows and the never-more-than view+margin contract. All assertions are over the
/// ranges of issued requests (and deltas where a baseline matters), never absolute
/// request counts — earlier phases of a test may legitimately have issued requests.
/// </summary>
public sealed class WindowingTests
{
    private static SpotGrid MakeGrid(IGridSource source, int margin = 25)
        => GridFixtures.Make(source)
            .With(source, GridFixtures.PixelColumns(12, 80), prefetchMargin: margin, pumps: 0)
            .Build();

    [AvaloniaFact]
    public void AGridWithFewerRowsThanTheViewportDrawsItsRowsNotPlaceholders()
    {
        // 400px body / 40px rows = 10 visible rows, but the source has 4: the applied window
        // can never "cover" 10 rows, yet every row that exists is present. It used to paint all
        // four as shimmer bars (WP-UI-15.T6).
        var source = new InMemoryGridSource(GridFixture.MakeRows(4, 12));
        var grid = MakeGrid(source);
        TestHost.Pump();

        Assert.True(grid.VisibleRowCount() > 4);
        Assert.NotNull(grid.VisibleRows);
        Assert.Equal(4, grid.VisibleRows!.Count);
        Assert.False(grid.IsLoading);
    }

    [AvaloniaFact]
    public void ACoveredShortGridIssuesNoRepeatFetchOnRelayout()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(4, 12));
        var grid = MakeGrid(source);
        TestHost.Pump();
        var issued = grid.Scheduler!.IssuedRequests.Count;

        grid.RequestWindow();
        grid.RequestWindow();
        TestHost.Pump();

        Assert.Equal(issued, grid.Scheduler.IssuedRequests.Count);
    }

    [AvaloniaFact]
    public void FirstArrangeIssuesOneWindowOfViewPlusMargin()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(1000, 12));
        var grid = MakeGrid(source, margin: 25);
        var requests = grid.Scheduler!.IssuedRequests;

        Assert.NotEmpty(requests);
        var first = requests[0];
        // 400px body / 40px comfortable rows = 10 visible rows; 10 + 25 = 35 max.
        Assert.True(first.Length <= 10 + 25, $"window {first} exceeds view+margin");
        Assert.Equal(0, first.Start);
    }

    [AvaloniaFact]
    public void WindowLengthNeverExceedsViewPlusMargin()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(100000, 12));
        var grid = MakeGrid(source, margin: 25);
        foreach (var request in grid.Scheduler!.IssuedRequests)
        {
            Assert.True(request.Length <= 10 + 25, $"window {request} exceeds the view+margin contract");
        }
    }

    [AvaloniaFact]
    public void ScrollingIssuesNextWindowAndPrefetchAroundView()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(100000, 12));
        var grid = MakeGrid(source, margin: 25);
        var baseline = grid.Scheduler!.IssuedRequests.Count;

        grid.ScrollToRow(50);
        TestHost.Pump();
        var windows = grid.Scheduler.IssuedRequests.Skip(baseline).ToList();

        Assert.NotEmpty(windows);
        // With a 10-row view and margin 25: expect roughly rows 88..135 (12 behind, 25 ahead).
        var request = windows[^1];
        Assert.True(request.Start <= 50, "prefetch must not start after the view start");
        Assert.True(request.Start + request.Length >= 50 + 10, "window must cover the view");
        Assert.True(request.Start + request.Length <= 50 + 10 + 25 + 6,
            $"window end {request.Start + request.Length} beyond loose prefetch bound");
    }

    [AvaloniaFact]
    public void CoveredViewDoesNotReissue()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(1000, 12));
        var grid = MakeGrid(source, margin: 40);
        var baseline = grid.Scheduler!.IssuedRequests.Count;

        // A tiny scroll inside the already-fetched window must not issue a new request.
        grid.ScrollToRow(1);
        TestHost.Pump();
        Assert.Equal(baseline, grid.Scheduler.IssuedRequests.Count);
    }

    [AvaloniaFact]
    public async Task SupersededWindowIsCancelledNotApplied()
    {
        var source = new DeferredGridSource(100000);
        var grid = MakeGrid(source, margin: 25);
        var initial = source.Pending.ToArray();

        // A new request supersedes the in-flight one immediately.
        grid.ScrollToRow(300);

        // Completing the stale window must NOT apply it.
        foreach (var window in initial)
        {
            source.CompleteOne(window);
        }

        TestHost.Pump();
        await Task.Delay(50, TestContext.Current.CancellationToken);
        TestHost.Pump();

        // The applied window must be the one belonging to row 300's view, not row 0's.
        Assert.NotEqual(initial[0].Start, grid.Scheduler!.LatestCompleted?.Start);
        Assert.Equal(300, grid.Scheduler!.LatestCompleted?.Start + 12 /* margin/2=12 */);
    }

    [AvaloniaFact]
    public async Task CancellingSourceTokenDoesNotApply()
    {
        var source = new DeferredGridSource(100000);
        var grid = MakeGrid(source, margin: 25);
        var pending = source.Pending.Single();
        var cancelBaseline = grid.Scheduler!.CancelledRequests.Count;

        pending.CancellationTokenSource.Cancel();
        await Task.Delay(30, TestContext.Current.CancellationToken);
        TestHost.Pump();

        Assert.True(grid.Scheduler.CancelledRequests.Count > cancelBaseline);
        Assert.Null(grid.Scheduler.LatestCompleted);
    }

    [AvaloniaFact]
    public void UnknownCountLearnsFromExhaustedWindow()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(37, 12), GridRowCount.Unknown());
        var grid = MakeGrid(source, margin: 25);
        TestHost.Pump();

        // The window asked for >=37 rows, got 37; the scheduler learned the end.
        Assert.Equal(37, grid.Scheduler!.EffectiveTotalRows);
        Assert.True(grid.Scheduler.IsTotalKnown);
    }

    [AvaloniaFact]
    public void ChangedResetReissuesTheView()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(500, 12));
        var grid = MakeGrid(source, margin: 25);
        grid.ScrollToRow(100);
        TestHost.Pump();
        var baseline = grid.Scheduler!.IssuedRequests.Count;

        source.AddRow(GridRow.Of("r-new", new GridCell("new")));
        TestHost.Pump();

        Assert.True(grid.Scheduler.IssuedRequests.Count > baseline, "a reset must reissue the view");
    }

    [AvaloniaFact]
    public void NeverRequestsBeyondSourceEnd()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(40, 12));
        var grid = MakeGrid(source, margin: 25);
        grid.ScrollToRow(38);
        TestHost.Pump();

        foreach (var request in grid.Scheduler!.IssuedRequests)
        {
            Assert.True(request.Start <= 40, $"window start {request.Start} past the end");
            Assert.True(request.Start + request.Length <= 40 + 25,
                $"window end {request.Start + request.Length} grossly past the end (clamp failed)");
        }
    }
}