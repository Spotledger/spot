using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 8's meta-cache behaviour over <see cref="FakeMetaHttpHandler"/>: the
/// first fetch parses the body, later fetches revalidate with
/// <see cref="System.Net.Http.Headers.HttpRequestHeaders"/> If-None-Match and a 304
/// keeps the held model, a changed version replaces the entry and raises
/// <see cref="MetaCache.MetaChanged"/> once, a contract error becomes an
/// <see cref="ApiRequestException"/> with the envelope's code, and a transport failure
/// retries before surfacing. Version bookkeeping rides the body's version.
/// </summary>
public sealed class MetaCacheTests
{
    [Fact]
    public async Task FirstFetchParsesBodyIntoModel()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);

        var model = await cache.GetAsync("sales_invoice");
        Assert.Equal("sales_invoice", model.Name);
        Assert.Equal("Sales Invoice", model.Label);
        Assert.Equal(7, model.Version);
        Assert.True(model.IsPosted);
        Assert.Equal(2, model.Fields.Count);
        Assert.Equal("customer", model.Field("customer")!.Name);
        Assert.Same(model, cache.Peek("sales_invoice"));
        Assert.Equal("\"7\"", cache.PeekETag("sales_invoice"));
        Assert.Equal(1, handler.Requests);
        Assert.Null(handler.LastIfNoneMatch); // the first fetch is unconditional
    }

    [Fact]
    public async Task RevalidationSendsQuotedIfNoneMatchAndA304KeepsTheModel()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        var first = await cache.GetAsync("sales_invoice");

        var changed = new List<string>();
        cache.MetaChanged += (_, doctype) => changed.Add(doctype);

        var second = await cache.GetAsync("sales_invoice");
        Assert.Equal("\"7\"", handler.LastIfNoneMatch); // the held (quoted) version echoed
        Assert.Equal(2, handler.Requests);
        Assert.Same(first, second); // the 304 keeps the exact model instance
        Assert.Empty(changed); // 304: never an event
    }

    [Fact]
    public async Task ChangedVersionReplacesTheEntryAndRaisesTheEventOnce()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        var first = await cache.GetAsync("sales_invoice");

        handler.MetaVersion = 8;
        handler.CurrentVersion = 8; // the held tag no longer matches: a 200 comes back
        var changed = new List<string>();
        cache.MetaChanged += (_, doctype) => changed.Add(doctype);

        var second = await cache.GetAsync("sales_invoice");
        Assert.Equal(8, second.Version);
        Assert.NotSame(first, second);
        Assert.Equal(8, cache.Peek("sales_invoice")!.Version);
        Assert.Single(changed);
        Assert.Equal("sales_invoice", changed.Single());
    }

    [Fact]
    public async Task SameVersion200ReplacesWithoutTheEvent()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        var first = await cache.GetAsync("sales_invoice");

        handler.CurrentVersion = 8; // the held tag no longer matches: a 200 comes back
        var changed = new List<string>();
        cache.MetaChanged += (_, doctype) => changed.Add(doctype);

        var second = await cache.GetAsync("sales_invoice");
        Assert.Equal(7, second.Version); // the body still says 7
        Assert.NotSame(first, second); // the entry was replaced
        Assert.Empty(changed); // but the version did not change: no event
    }

    [Fact]
    public async Task VersionBookkeepingRidesTheBodyNotTheEtagHeader()
    {
        // ServeStaleBody: the ETag header carries MetaVersion (9) while the body
        // carries CurrentVersion (8). The cache must trust the BODY — the next fetch
        // then revalidates with "8" and the fake server (whose current version is 8)
        // answers 304. With the header as bookkeeping it would send "9" and fetch again.
        using var connection = ConnectedConnection(out var handler);
        handler.MetaVersion = 9; // the ETag header the fake sends says "9"
        handler.ServeStaleBody = true; // ... but the body's version says 7 (CurrentVersion)
        var cache = new MetaCache(connection);

        var first = await cache.GetAsync("sales_invoice");
        Assert.Equal(7, first.Version); // the BODY version, not the header's 9
        Assert.Equal("\"7\"", cache.PeekETag("sales_invoice")); // bookkeeping rides the body

        var second = await cache.GetAsync("sales_invoice");
        Assert.Equal("\"7\"", handler.LastIfNoneMatch); // revalidates with the body's 7
        Assert.Same(first, second); // the fake answers 304 to "7" (its current version)
        Assert.Equal(2, handler.Requests);
    }

    [Fact]
    public async Task ContractErrorBecomesApiRequestExceptionWithoutARetry()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        handler.ServeError = (404, "doctype.unknown", "No such DocType.");

        var exception = await Assert.ThrowsAsync<ApiRequestException>(() => cache.GetAsync("sales_invoice"));
        Assert.Equal("doctype.unknown", exception.Error.Code);
        Assert.Equal("No such DocType.", exception.Error.Message);
        Assert.Equal(404, exception.Error.StatusCode);
        Assert.Equal(1, handler.Requests); // a contract error is never retried
    }

    [Fact]
    public async Task TransportFailuresAreRetriedAndThenSucceed()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        handler.RemainingTransportFailures = 2; // attempts 1 and 2 drop, attempt 3 answers

        var model = await cache.GetAsync("sales_invoice");
        Assert.Equal("sales_invoice", model.Name);
        Assert.Equal(3, handler.Requests);
        Assert.Equal(MetaCacheRetryPolicy.MaxAttempts, handler.Requests);
    }

    [Fact]
    public async Task ExhaustedTransportFailuresSurfaceTheError()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        handler.RemainingTransportFailures = MetaCacheRetryPolicy.MaxAttempts; // every attempt drops

        await Assert.ThrowsAsync<System.Net.Http.HttpRequestException>(() => cache.GetAsync("sales_invoice"));
        Assert.Equal(MetaCacheRetryPolicy.MaxAttempts, handler.Requests);
    }

    [Fact]
    public async Task DisconnectedConnectionThrowsTheConnectionOwnsGuard()
    {
        using var connection = new ApiConnection(new FakeMetaHttpHandler());
        var cache = new MetaCache(connection);
        await Assert.ThrowsAsync<InvalidOperationException>(() => cache.GetAsync("sales_invoice"));
    }

    [Fact]
    public async Task InvalidateForgetsTheEntryAndTheNextFetchIsUnconditional()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        await cache.GetAsync("sales_invoice");
        Assert.NotNull(cache.Peek("sales_invoice"));

        cache.Invalidate("sales_invoice");
        Assert.Null(cache.Peek("sales_invoice"));
        Assert.Null(cache.PeekETag("sales_invoice"));

        await cache.GetAsync("sales_invoice");
        Assert.Null(handler.LastIfNoneMatch); // the invalidate made the next fetch unconditional
        Assert.Equal(2, handler.Requests);
    }

    [Fact]
    public async Task ClearForgetsEverything()
    {
        using var connection = ConnectedConnection(out var handler);
        var cache = new MetaCache(connection);
        await cache.GetAsync("sales_invoice");

        cache.Clear();
        Assert.Null(cache.Peek("sales_invoice"));
        Assert.Null(cache.PeekETag("sales_invoice"));
    }

    private static ApiConnection ConnectedConnection(out FakeMetaHttpHandler handler)
    {
        handler = new FakeMetaHttpHandler { MetaVersion = 7, CurrentVersion = 7 };
        var connection = new ApiConnection(handler);
        connection.Connect(new Uri("https://spot.test"));
        return connection;
    }
}