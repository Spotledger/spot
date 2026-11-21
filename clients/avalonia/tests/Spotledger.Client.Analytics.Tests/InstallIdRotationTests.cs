using Xunit;

namespace Spotledger.Client.Analytics.Tests;

/// <summary>
/// The install id must not survive a withdrawal: two opt-out/opt-in cycles have to yield three
/// different ids, and a batch that is being sent when the user withdraws must not be delivered.
/// </summary>
public sealed class InstallIdRotationTests
{
    private static readonly DateOnly Day = new(2026, 10, 23);

    private static readonly KnownScreens Screens = KnownScreens.From(["a.b"]);

    [Fact]
    public async Task Two_opt_out_cycles_yield_three_different_install_ids()
    {
        var consent = new AnalyticsConsent(true);
        var sink = new InMemoryAnalyticsSink();
        var store = new InMemoryInstallIdStore();
        using var analytics = new UsageAnalytics(consent, sink, store, Screens);

        var seen = new List<string>();
        for (var cycle = 0; cycle < 3; cycle++)
        {
            analytics.ScreenOpened("a.b");
            Assert.Equal(FlushResult.Sent, await analytics.FlushAsync(Day));
            seen.Add(sink.Sent[^1].InstallId);

            if (cycle == 2)
            {
                continue; // leave the third cycle opted in, so the store can be checked below
            }

            consent.IsEnabled = false;
            Assert.Equal(FlushResult.ConsentOff, await analytics.FlushAsync(Day));
            Assert.Null(store.Stored); // the id is discarded, not kept for next time
            consent.IsEnabled = true;
        }

        Assert.Equal(3, seen.Count);
        Assert.Equal(3, seen.Distinct(StringComparer.Ordinal).Count());
        Assert.All(seen, id => Assert.True(InstallId.IsValid(id)));
        Assert.Equal(seen[^1], store.Stored);
    }

    [Fact]
    public async Task An_id_created_before_the_opt_out_is_never_used_again()
    {
        var consent = new AnalyticsConsent(true);
        var sink = new InMemoryAnalyticsSink();
        var store = InMemoryInstallIdStore.Seeded(InstallId.Create());
        var first = store.Stored!;
        using var analytics = new UsageAnalytics(consent, sink, store, Screens);

        analytics.ScreenOpened("a.b");
        await analytics.FlushAsync(Day);
        Assert.Equal(first, sink.Sent[0].InstallId);

        analytics.Clear(); // the Shell's withdrawal path
        consent.IsEnabled = false;
        consent.IsEnabled = true;
        analytics.ScreenOpened("a.b");
        await analytics.FlushAsync(Day);

        Assert.Equal(2, sink.Sent.Count);
        Assert.NotEqual(sink.Sent[0].InstallId, sink.Sent[1].InstallId);
        Assert.NotEqual(first, store.Stored);
    }

    [Fact]
    public void Clear_discards_the_install_id_and_the_counters()
    {
        var store = InMemoryInstallIdStore.Seeded(InstallId.Create());
        using var analytics = new UsageAnalytics(new AnalyticsConsent(true), new InMemoryAnalyticsSink(), store, Screens);
        analytics.ScreenOpened("a.b");

        analytics.Clear();

        Assert.Null(store.Stored);
        Assert.True(analytics.PendingPayload(Day).IsEmpty);
    }

    [Fact]
    public async Task A_batch_in_flight_when_the_user_withdraws_is_not_sent()
    {
        var consent = new AnalyticsConsent(true);
        var sink = new BlockingSink();
        using var analytics = new UsageAnalytics(consent, sink, new InMemoryInstallIdStore(), Screens);
        analytics.ScreenOpened("a.b");

        var flush = analytics.FlushAsync(Day);
        await sink.Started.Task; // the sink is inside SendAsync and has not completed

        analytics.Clear(); // withdrawal while the batch is in flight
        consent.IsEnabled = false;

        Assert.Equal(FlushResult.ConsentOff, await flush);
        Assert.Equal(0, sink.Delivered);
    }

    [Fact]
    public async Task An_id_stored_while_consent_is_off_at_start_up_is_not_reused()
    {
        var consent = new AnalyticsConsent(false);
        var sink = new InMemoryAnalyticsSink();
        var first = InstallId.Create();
        var store = InMemoryInstallIdStore.Seeded(first);
        using var analytics = new UsageAnalytics(consent, sink, store, Screens);

        Assert.Null(store.Stored);
        consent.IsEnabled = true;
        analytics.ScreenOpened("a.b");
        Assert.Equal(FlushResult.Sent, await analytics.FlushAsync(Day));
        Assert.NotEqual(first, sink.Sent[0].InstallId);
    }

    [Fact]
    public void The_constructor_discards_a_stored_id_that_is_not_valid()
    {
        var store = new CorruptStore("jane@example.com");
        using var analytics = new UsageAnalytics(new AnalyticsConsent(true), new InMemoryAnalyticsSink(), store, Screens);

        Assert.True(InstallId.IsValid(analytics.PendingPayload(Day).InstallId));
        Assert.True(store.Discarded);
    }

    private sealed class CorruptStore : IInstallIdStore
    {
        private string? _id;

        public CorruptStore(string id)
        {
            _id = id;
        }

        public bool Discarded { get; private set; }

        public string? Read() => _id;

        public void Store(string installId) => _id = installId;

        public void Discard()
        {
            _id = null;
            Discarded = true;
        }
    }

    private sealed class BlockingSink : IAnalyticsSink
    {
        public TaskCompletionSource Started { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);

        public int Delivered { get; private set; }

        public async ValueTask SendAsync(AnalyticsPayload payload, CancellationToken cancellationToken)
        {
            Started.TrySetResult();
            await Task.Delay(Timeout.Infinite, cancellationToken);
            Delivered++;
        }
    }
}
