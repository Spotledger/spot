using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Analytics.Tests;

public sealed class UsageAnalyticsTests
{
    private static readonly DateOnly Day = new(2026, 10, 23);

    private sealed class Fixture
    {
        public Fixture(params string[] screens)
        {
            Store = InMemoryInstallIdStore.Seeded(Install);
            Analytics = new UsageAnalytics(Consent, Sink, Store, KnownScreens.From(screens.Length == 0 ? ["a.b", "sales.invoice.form"] : screens));
        }

        public AnalyticsConsent Consent { get; } = new(true);

        public InMemoryAnalyticsSink Sink { get; } = new();

        public string Install { get; } = InstallId.Create();

        public InMemoryInstallIdStore Store { get; }

        public UsageAnalytics Analytics { get; }
    }

    [Fact]
    public async Task Events_aggregate_into_counters_and_flush_as_one_batch()
    {
        var f = new Fixture();
        f.Analytics.ScreenOpened("sales.invoice.form");
        f.Analytics.ScreenOpened("sales.invoice.form");
        f.Analytics.ActionUsed("sales.invoice.form", "save");
        f.Analytics.ActionUsed("sales.invoice.form", "save");
        f.Analytics.ActionUsed("sales.invoice.form", "submit");
        f.Analytics.ErrorShown("SP-1042");
        f.Analytics.ShortcutUsed("sales.invoice.form", "form.save");

        Assert.Equal(FlushResult.Sent, await f.Analytics.FlushAsync(Day));

        var batch = Assert.Single(f.Sink.Sent);
        Assert.Equal(Day, batch.Day);
        Assert.Equal(f.Install, batch.InstallId);
        Assert.Equal(5, batch.Counters.Count);
        Assert.Equal(2, Count(batch, AnalyticsEvent.ScreenOpened, "sales.invoice.form", null));
        Assert.Equal(2, Count(batch, AnalyticsEvent.ActionUsed, "sales.invoice.form", "save"));
        Assert.Equal(1, Count(batch, AnalyticsEvent.ActionUsed, "sales.invoice.form", "submit"));
        Assert.Equal(1, Count(batch, AnalyticsEvent.ErrorShown, null, "SP-1042"));
        Assert.Equal(1, Count(batch, AnalyticsEvent.ShortcutUsed, "sales.invoice.form", "form.save"));

        // sent counters are cleared: a second flush has nothing to send
        Assert.Equal(FlushResult.NothingToSend, await f.Analytics.FlushAsync(Day));
        Assert.Single(f.Sink.Sent);
    }

    [Fact]
    public async Task Consent_off_records_nothing_and_sends_nothing()
    {
        var f = new Fixture();
        f.Consent.IsEnabled = false;

        Assert.False(f.Analytics.ScreenOpened("a.b"));
        Assert.False(f.Analytics.ActionUsed("a.b", "save"));
        Assert.False(f.Analytics.ErrorShown("E1"));
        Assert.False(f.Analytics.ShortcutUsed("a.b", "x"));
        Assert.True(f.Analytics.PendingPayload(Day).IsEmpty);
        Assert.Equal(FlushResult.ConsentOff, await f.Analytics.FlushAsync(Day));
        Assert.Empty(f.Sink.Sent);
    }

    [Fact]
    public async Task Opting_out_discards_what_was_held()
    {
        var f = new Fixture();
        f.Analytics.ScreenOpened("a.b");
        f.Consent.IsEnabled = false;
        Assert.Equal(FlushResult.ConsentOff, await f.Analytics.FlushAsync(Day));

        // opting back in does not resurrect the old counters
        f.Consent.IsEnabled = true;
        Assert.True(f.Analytics.PendingPayload(Day).IsEmpty);
        Assert.Equal(FlushResult.NothingToSend, await f.Analytics.FlushAsync(Day));
        Assert.Empty(f.Sink.Sent);
    }

    [Fact]
    public void Consent_defaults_to_off()
    {
        Assert.False(new AnalyticsConsent().IsEnabled);
    }

    [Fact]
    public async Task A_failed_send_keeps_counters_and_events_during_the_send_are_not_lost()
    {
        var consent = new AnalyticsConsent(true);
        var sink = new ScriptedSink();
        using var analytics = new UsageAnalytics(consent, sink, InstallId.Create(), KnownScreens.From(["a.b"]));
        analytics.ScreenOpened("a.b");

        sink.Fail = true;
        Assert.Equal(FlushResult.Failed, await analytics.FlushAsync(Day));
        Assert.Equal(1, Assert.Single(analytics.PendingPayload(Day).Counters).Count);

        sink.Fail = false;
        sink.OnSend = () => analytics.ScreenOpened("a.b"); // recorded while the batch is in flight
        Assert.Equal(FlushResult.Sent, await analytics.FlushAsync(Day));
        Assert.Equal(1, Assert.Single(sink.Batches).Counters[0].Count);
        Assert.Equal(1, Assert.Single(analytics.PendingPayload(Day).Counters).Count); // the in-flight event stays
    }

    [Fact]
    public async Task Cancellation_propagates()
    {
        using var analytics = new UsageAnalytics(new AnalyticsConsent(true), new ScriptedSink { Cancel = true }, InstallId.Create(), KnownScreens.From(["a.b"]));
        analytics.ScreenOpened("a.b");
        await Assert.ThrowsAsync<OperationCanceledException>(() => analytics.FlushAsync(Day));
    }

    [Fact]
    public void Invalid_ids_are_dropped_not_thrown()
    {
        var f = new Fixture();
        Assert.False(f.Analytics.ScreenOpened("Has Space"));
        Assert.False(f.Analytics.ScreenOpened(""));
        Assert.False(f.Analytics.ScreenOpened(new string('a', 65)));
        Assert.False(f.Analytics.ActionUsed("a.b", "jane@example.com"));
        Assert.False(f.Analytics.ErrorShown("Invoice 1042 not found"));
        Assert.False(f.Analytics.ShortcutUsed("a.b", "C:\\Users\\jane"));
        Assert.True(f.Analytics.PendingPayload(Day).IsEmpty);

        // not screen-id shapes at all, so none of them is counted as a rejected screen id
        Assert.Equal(0, f.Analytics.RejectedCount);
    }

    [Fact]
    public void Counter_table_is_bounded_and_overflow_is_counted()
    {
        var ids = Enumerable.Range(0, UsageAnalytics.MaxCounters + 5).Select(i => "s" + i).ToArray();
        var f = new Fixture(ids);
        for (var i = 0; i < UsageAnalytics.MaxCounters + 5; i++)
        {
            f.Analytics.ScreenOpened("s" + i);
        }

        var p = f.Analytics.PendingPayload(Day);
        Assert.Equal(UsageAnalytics.MaxCounters, p.Counters.Count);
        Assert.Equal(5, p.Dropped);
        Assert.True(f.Analytics.ScreenOpened("s0")); // existing keys still count
    }

    [Fact]
    public void Inspector_shows_exactly_what_would_be_sent()
    {
        var f = new Fixture();
        f.Analytics.ActionUsed("a.b", "save");
        using var doc = JsonDocument.Parse(f.Analytics.InspectJson(Day));
        var root = doc.RootElement;
        Assert.Equal(1, root.GetProperty("schema_version").GetInt32());
        Assert.Equal(f.Install, root.GetProperty("install_id").GetString());
        Assert.Equal("2026-10-23", root.GetProperty("day").GetString());
        Assert.Equal(0, root.GetProperty("rejected").GetInt64());
        var c = root.GetProperty("counters")[0];
        Assert.Equal("action_used", c.GetProperty("event").GetString());
        Assert.Equal("a.b", c.GetProperty("screen_id").GetString());
        Assert.Equal("save", c.GetProperty("action_id").GetString());
        Assert.Equal(1, c.GetProperty("count").GetInt64());
    }

    [Fact]
    public void Install_id_is_random_hex_and_validated()
    {
        var a = InstallId.Create();
        Assert.True(InstallId.IsValid(a));
        Assert.NotEqual(a, InstallId.Create());
        Assert.False(InstallId.IsValid("jane@example.com"));
        Assert.False(InstallId.IsValid(new string('A', 32)));
        Assert.Throws<ArgumentException>(() => new UsageAnalytics(new AnalyticsConsent(true), new InMemoryAnalyticsSink(), "user-42", KnownScreens.From(["a.b"])));
    }

    [Fact]
    public async Task An_unknown_screen_id_is_dropped_and_counted_but_never_echoed()
    {
        var f = new Fixture();
        const string Hostile = "acme-invoice-0042-form";

        Assert.False(f.Analytics.ScreenOpened(Hostile));
        Assert.False(f.Analytics.ActionUsed(Hostile, "save"));
        Assert.False(f.Analytics.ShortcutUsed(Hostile, "form.save"));
        Assert.Equal(3, f.Analytics.RejectedCount);

        var payload = f.Analytics.PendingPayload(Day);
        Assert.Empty(payload.Counters);
        Assert.Equal(3, payload.Rejected);
        Assert.DoesNotContain("acme", f.Analytics.InspectJson(Day), StringComparison.Ordinal);

        // the count travels as a number and is cleared with the batch; the text never was anywhere
        Assert.Equal(FlushResult.Sent, await f.Analytics.FlushAsync(Day));
        Assert.Equal(3, Assert.Single(f.Sink.Sent).Rejected);
        Assert.Equal(0, f.Analytics.RejectedCount);
    }

    [Fact]
    public async Task An_empty_known_screen_set_rejects_everything()
    {
        using var analytics = new UsageAnalytics(new AnalyticsConsent(true), new InMemoryAnalyticsSink(), InstallId.Create(), KnownScreens.Empty);

        Assert.False(analytics.ScreenOpened("a.b"));
        Assert.False(analytics.ActionUsed("a.b", "save"));
        Assert.False(analytics.ShortcutUsed("a.b", "form.save"));
        Assert.Empty(analytics.PendingPayload(Day).Counters);
        Assert.Equal(3, analytics.PendingPayload(Day).Rejected);

        // error codes carry no screen id, so they are unaffected
        Assert.True(analytics.ErrorShown("SP-1"));
        Assert.Equal(FlushResult.Sent, await analytics.FlushAsync(Day));
    }

    [Fact]
    public async Task No_ingest_token_reports_NoIngestToken_and_keeps_the_batch_local()
    {
        var handler = new SilentHandler();
        using var sink = new HttpAnalyticsSink(() => FixedAnalyticsTokenSource.None, new Uri("https://example.test/"), transport: handler);
        using var analytics = new UsageAnalytics(new AnalyticsConsent(true), sink, InstallId.Create(), KnownScreens.From(["a.b"]));
        analytics.ScreenOpened("a.b");

        Assert.Equal(FlushResult.NoIngestToken, await analytics.FlushAsync(Day));
        Assert.Equal(0, handler.Calls);
        Assert.Equal(1, Assert.Single(analytics.PendingPayload(Day).Counters).Count);
    }

    [Fact]
    public async Task Http_sink_posts_the_json_and_fails_on_error_status()
    {
        var handler = new SilentHandler();
        using var sink = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource("deployment-ingest-token-0001"), new Uri("https://example.test/"), transport: handler);
        using var analytics = new UsageAnalytics(new AnalyticsConsent(true), sink, InstallId.Create(), KnownScreens.From(["a.b"]));
        analytics.ScreenOpened("a.b");

        Assert.Equal(FlushResult.Sent, await analytics.FlushAsync(Day));
        Assert.Equal("/api/telemetry/usage", handler.Path);
        Assert.Equal("application/json", handler.ContentType);
        Assert.Contains("\"screen_opened\"", handler.Body, StringComparison.Ordinal);

        handler.Status = System.Net.HttpStatusCode.InternalServerError;
        analytics.ScreenOpened("a.b");
        Assert.Equal(FlushResult.Failed, await analytics.FlushAsync(Day));
    }

    private static long Count(AnalyticsPayload p, AnalyticsEvent e, string? screen, string? item) =>
        p.Counters.Single(c => c.Event == e && c.ScreenId == screen && c.ItemId == item).Count;

    private sealed class ScriptedSink : IAnalyticsSink
    {
        public bool Fail { get; set; }

        public bool Cancel { get; set; }

        public Action? OnSend { get; set; }

        public List<AnalyticsPayload> Batches { get; } = [];

        public ValueTask SendAsync(AnalyticsPayload payload, CancellationToken cancellationToken)
        {
            if (Cancel)
            {
                throw new OperationCanceledException();
            }

            if (Fail)
            {
                throw new InvalidOperationException("down");
            }

            Batches.Add(payload);
            OnSend?.Invoke();
            return ValueTask.CompletedTask;
        }
    }

    private sealed class SilentHandler : HttpMessageHandler
    {
        public System.Net.HttpStatusCode Status { get; set; } = System.Net.HttpStatusCode.NoContent;

        public int Calls { get; private set; }

        public string Path { get; private set; } = "";

        public string ContentType { get; private set; } = "";

        public string Body { get; private set; } = "";

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            Calls++;
            Path = request.RequestUri!.AbsolutePath;
            ContentType = request.Content!.Headers.ContentType!.MediaType!;
            Body = await request.Content.ReadAsStringAsync(cancellationToken);
            return new HttpResponseMessage(Status);
        }
    }
}
