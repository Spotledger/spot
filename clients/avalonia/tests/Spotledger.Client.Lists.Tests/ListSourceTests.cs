using System.Text.Json.Nodes;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

public sealed class ListSourceTests
{
    internal static ListColumnSet Columns(params string[] names)
    {
        var model = Fixtures.Model();
        var state = Fixtures.Permissions().DocTypes["order"];
        var layout = names.Select(n => (model.Field(n)!, (double?)null)).ToArray();
        return new ListColumnSet(ListColumnPlanner.Build(layout, state), new ListCellFormatter(System.Globalization.CultureInfo.InvariantCulture, TimeZoneInfo.Utc));
    }

    private static readonly int[] Starts = [0, 400, 150, 300, 20, 450];

    private static ListSource Source(FakeListGateway gateway, ListSourceOptions? options = null, ListFilter? filter = null)
        => new(gateway, "order", Columns("title", "customer", "amount", "salary"), filter, null, options);

    private static JsonNode Request(FakeListGateway gateway, int index) => JsonNode.Parse(gateway.Requests[index])!;

    [Fact]
    public async Task FirstPageAsksForTheColumnsAndNoTotal()
    {
        var gateway = new FakeListGateway(1000);
        var source = Source(gateway);

        var rows = await source.GetWindow(0, 40, CancellationToken.None);

        Assert.Equal(40, rows.Count);
        Assert.Single(gateway.Requests);
        var request = Request(gateway, 0);
        Assert.Equal(100, (int)request["limit"]!);
        Assert.Null(request["cursor"]);
        Assert.Null(request["with_total"]);
        Assert.Equal(["title", "customer", "amount", "salary"], request["fields"]!.AsArray().Select(f => (string)f!).ToArray());
        Assert.Equal("d-000000", rows[0].Key);
        Assert.Equal("Order 0", rows[0].Cells[0].Text);
        Assert.Equal("Party p-0", rows[0].Cells[1].Text);
        Assert.Equal("5.50", rows[0].Cells[2].Text);
        Assert.Equal(GridCellAlignment.End, rows[0].Cells[2].Alignment);
    }

    [Fact]
    public async Task AWindowFarDownWalksTheCursorChainOnceAndKeepsWhatItWalked()
    {
        var gateway = new FakeListGateway(1000);
        var source = Source(gateway);

        var rows = await source.GetWindow(250, 50, CancellationToken.None);

        Assert.Equal(50, rows.Count);
        Assert.Equal("d-000250", rows[0].Key);
        Assert.Equal(3, gateway.Requests.Count);
        Assert.Null(Request(gateway, 0)["cursor"]);
        Assert.Equal("c:100", (string)Request(gateway, 1)["cursor"]!);
        Assert.Equal("c:200", (string)Request(gateway, 2)["cursor"]!);

        await source.GetWindow(120, 10, CancellationToken.None);
        await source.GetWindow(0, 10, CancellationToken.None);
        Assert.Equal(3, gateway.Requests.Count);
        Assert.Equal(1, gateway.MaxInFlight);
    }

    [Fact]
    public async Task AWindowSpanningTwoPagesIsOneContiguousSlice()
    {
        var gateway = new FakeListGateway(1000);
        var source = Source(gateway);

        var rows = await source.GetWindow(90, 30, CancellationToken.None);

        Assert.Equal(Enumerable.Range(90, 30).Select(i => $"d-{i:D6}"), rows.Select(r => (string)r.Key));
    }

    [Fact]
    public async Task CountIsEstimatedWithMoreAndBecomesKnownAtTheLastPage()
    {
        var gateway = new FakeListGateway(250);
        var source = Source(gateway);

        await source.GetWindow(0, 10, CancellationToken.None);
        Assert.Equal(GridRowCountKind.Estimated, source.Count.Kind);
        Assert.True(source.HasMore);
        Assert.Null(source.ExactTotal);

        await source.GetWindow(200, 50, CancellationToken.None);
        Assert.Equal(GridRowCount.Known(250), source.Count);
        Assert.False(source.HasMore);
        Assert.Equal(250, source.ExactTotal);
        Assert.DoesNotContain(gateway.Requests, r => r.Contains("with_total", StringComparison.Ordinal));
    }

    [Fact]
    public async Task AWindowPastTheEndIsEmptyAndTheEndIsRemembered()
    {
        var gateway = new FakeListGateway(150);
        var source = Source(gateway);

        var rows = await source.GetWindow(400, 20, CancellationToken.None);
        var before = gateway.Requests.Count;
        var again = await source.GetWindow(500, 20, CancellationToken.None);

        Assert.Empty(rows);
        Assert.Empty(again);
        Assert.Equal(before, gateway.Requests.Count);
        Assert.Equal(150, source.ExactTotal);
    }

    [Fact]
    public async Task ExactPolicyAsksForTheTotalOnTheFirstPageOnly()
    {
        var gateway = new FakeListGateway(5000);
        var source = Source(gateway, new ListSourceOptions { Count = CountPolicy.Exact });

        await source.GetWindow(150, 10, CancellationToken.None);

        Assert.Equal(GridRowCount.Known(5000), source.Count);
        Assert.True((bool)Request(gateway, 0)["with_total"]!);
        Assert.Null(Request(gateway, 1)["with_total"]);
    }

    [Fact]
    public async Task AnExactCountIsOnlyFetchedWhenAskedFor()
    {
        var gateway = new FakeListGateway(7000);
        var source = Source(gateway);
        await source.GetWindow(0, 10, CancellationToken.None);

        var total = await source.RequestExactCountAsync(CancellationToken.None);
        var again = await source.RequestExactCountAsync(CancellationToken.None);

        Assert.Equal(7000, total);
        Assert.Equal(7000, again);
        Assert.Equal(2, gateway.Requests.Count);
        Assert.Equal(1, (int)Request(gateway, 1)["limit"]!);
        Assert.True((bool)Request(gateway, 1)["with_total"]!);
    }

    [Fact]
    public async Task AResponseForAnOlderQueryIsDroppedNeverShown()
    {
        var gateway = new FakeListGateway(1000) { Prefix = "old " };
        var release = new TaskCompletionSource();
        gateway.Hold = (index, _, token) => index == 0 ? release.Task.WaitAsync(token) : Task.CompletedTask;
        var source = Source(gateway);

        var stale = source.GetWindow(0, 20, CancellationToken.None);
        await Task.Delay(30, TestContext.Current.CancellationToken);
        gateway.Prefix = "new ";
        source.Apply(new TextFilter("title", TextOp.Contains, "x", true), null);
        var fresh = await source.GetWindow(0, 20, CancellationToken.None);
        release.TrySetResult();

        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => stale);
        Assert.StartsWith("new ", fresh[0].Cells[0].Text, StringComparison.Ordinal);
        Assert.Equal(1, gateway.MaxInFlight);
    }

    [Fact]
    public async Task AResponseThatArrivesAfterTheQueryChangedIsNotCached()
    {
        var gateway = new FakeListGateway(1000);
        var arrive = new TaskCompletionSource();
        gateway.Hold = (index, _, _) => index == 0 ? arrive.Task : Task.CompletedTask;
        var source = Source(gateway);

        var stale = source.GetWindow(0, 20, CancellationToken.None);
        await Task.Delay(30, TestContext.Current.CancellationToken);
        source.Refresh();
        arrive.SetResult();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => stale);

        Assert.Empty(source.CachedRows());
    }

    [Fact]
    public async Task ASupersededWindowIsCancelledThroughItsToken()
    {
        var gateway = new FakeListGateway(1000) { Latency = TimeSpan.FromSeconds(5) };
        var source = Source(gateway);
        using var cts = new CancellationTokenSource();

        var task = source.GetWindow(0, 20, cts.Token);
        await Task.Delay(30, TestContext.Current.CancellationToken);
        cts.Cancel();

        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => task);
        Assert.Empty(source.CachedRows());
    }

    [Fact]
    public async Task ConcurrentWindowsNeverMakeConcurrentQueries()
    {
        var gateway = new FakeListGateway(5000) { Latency = TimeSpan.FromMilliseconds(5) };
        var source = Source(gateway);

        var tasks = Starts.Select(start => source.GetWindow(start, 30, CancellationToken.None)).ToArray();
        await Task.WhenAll(tasks);

        Assert.Equal(1, gateway.MaxInFlight);
        Assert.All(tasks, t => Assert.Equal(30, t.Result.Count));
    }

    [Fact]
    public async Task SortAndFilterGoToTheServerAsStructuredData()
    {
        var gateway = new FakeListGateway(1000);
        var source = Source(gateway);
        await source.GetWindow(0, 10, CancellationToken.None);

        await source.Sort(2, GridSortDirection.Descending);
        source.Apply(new CompareFilter("amount", CompareOp.Ge, ListScalar.Of("10.00")), source.Order);
        await source.GetWindow(0, 10, CancellationToken.None);

        var last = Request(gateway, gateway.Requests.Count - 1);
        Assert.Equal("""[{"field":"amount","dir":"desc"}]""", last["order"]!.ToJsonString());
        Assert.Equal("""{"field":"amount","op":"ge","value":"10.00"}""", last["filter"]!.ToJsonString());
        Assert.Null(last["cursor"]);

        await source.Sort(2, GridSortDirection.None);
        await source.GetWindow(0, 10, CancellationToken.None);
        Assert.Null(Request(gateway, gateway.Requests.Count - 1)["order"]);
    }

    [Fact]
    public async Task ARefusedFilterNeverSendsAQuery()
    {
        var gateway = new FakeListGateway(1000);
        var source = Source(gateway);
        var wide = new AndFilter(Enumerable.Range(0, 30).Select(_ => (ListFilter)new NullFilter("title", true)).ToArray());

        Assert.Throws<ArgumentException>(() => source.Apply(wide, null));
        await source.GetWindow(0, 10, CancellationToken.None);

        Assert.DoesNotContain(gateway.Requests, r => r.Contains("\"and\"", StringComparison.Ordinal));
    }

    [Fact]
    public async Task LinkLabelsAreResolvedInOneBatchPerPageAndNotAgain()
    {
        var gateway = new FakeListGateway(1000);
        var source = Source(gateway);

        await source.GetWindow(0, 10, CancellationToken.None);
        await source.GetWindow(100, 10, CancellationToken.None);

        // The second page only holds parties the first page already resolved: no second lookup.
        Assert.Single(gateway.LabelCalls);
        Assert.Equal("party", gateway.LabelCalls[0].Target);
        Assert.Equal(7, gateway.LabelCalls[0].Ids.Length);
    }

    [Fact]
    public async Task VersionsAreKeptForDelete()
    {
        var gateway = new FakeListGateway(1000);
        var source = Source(gateway);

        await source.GetWindow(0, 5, CancellationToken.None);

        Assert.Equal(4, source.VersionOf("d-000003"));
        Assert.Null(source.VersionOf("d-999999"));
    }

    [Fact]
    public async Task AFailedQueryRaisesTheFailureAndKeepsTheCacheClean()
    {
        var gateway = new FakeListGateway(1000) { FailNext = new Api.ApiRequestException(Fixtures.Error("api.unavailable", "Down", 503)) };
        var source = Source(gateway);
        Exception? seen = null;
        source.QueryFailed += (_, e) => seen = e;

        await Assert.ThrowsAsync<Api.ApiRequestException>(() => source.GetWindow(0, 10, CancellationToken.None));
        var rows = await source.GetWindow(0, 10, CancellationToken.None);

        Assert.NotNull(seen);
        Assert.Equal(10, rows.Count);
    }

    [Fact]
    public async Task OldPagesAreEvictedSoMemoryStaysBounded()
    {
        var gateway = new FakeListGateway(5000);
        var source = Source(gateway, new ListSourceOptions { PageSize = 50, MaxCachedPages = 3 });

        await source.GetWindow(900, 10, CancellationToken.None);

        Assert.True(source.CachedRows().Count <= 150);
    }
}
