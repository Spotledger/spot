using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>Reconnect: close-code handling, resume, backoff and the heartbeat.</summary>
public sealed class ReconnectTests
{
    private const string Invoices = "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f";
    private const string Partners = "doctype:customer";

    [Theory]
    [InlineData(4001)]
    [InlineData(4009)]
    public async Task The_session_ending_codes_stop_reconnecting_and_end_the_session(int closeCode)
    {
        await using var harness = RealtimeHarness.Create();
        var ended = new TaskCompletionSource<RealtimeSessionEndedEventArgs>(TaskCreationOptions.RunContinuationsAsynchronously);
        harness.Client.SessionEnded += (_, args) => ended.TrySetResult(args);
        var connection = await harness.ConnectAsync();

        await connection.CloseAsync(closeCode);

        var session = await ended.Task.WithTimeoutAsync("the session-ended event");
        Assert.Equal(closeCode, session.CloseCode);
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.SignedOut, "the signed-out state");
        // Deliberately no timer pending: the client stopped, so this proves nothing is scheduled.
        harness.Clock.Advance(TimeSpan.FromMinutes(5));
        Assert.Equal(1, harness.Server.ConnectionCount);
        Assert.DoesNotContain(
            harness.Diagnostics,
            line => line.Contains("session-token-under-test", StringComparison.Ordinal));
    }

    [Theory]
    [InlineData(1003)]
    [InlineData(1008)]
    [InlineData(1009)]
    public async Task The_client_bug_codes_log_once_and_stop(int closeCode)
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        await connection.CloseAsync(closeCode);

        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Closed, "the closed state");
        // Deliberately no timer pending: the client stopped, so this proves nothing is scheduled.
        harness.Clock.Advance(TimeSpan.FromMinutes(5));
        Assert.Equal(1, harness.Server.ConnectionCount);
        Assert.Contains(harness.Diagnostics, line => line.Contains("fatal protocol fault", StringComparison.Ordinal));
    }

    [Fact]
    public async Task A_normal_close_stops_without_reconnecting()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        await connection.CloseAsync(1000);

        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Closed, "the closed state");
        // Deliberately no timer pending: the client stopped, so this proves nothing is scheduled.
        harness.Clock.Advance(TimeSpan.FromMinutes(5));
        Assert.Equal(1, harness.Server.ConnectionCount);
    }

    [Fact]
    public async Task A_transport_fault_reconnects_after_the_backoff()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        connection.Abort();

        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Reconnecting, "the reconnect wait");
        await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");
        var retry = await harness.Server.WaitForConnectionAsync(1);
        await retry.ReadAuthAsync();
        await retry.SendReadyAsync();
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Ready, "the second ready");
    }

    [Fact]
    public async Task A_slow_consumer_close_reconnects_and_resumes_every_room_with_its_own_seq()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var invoices = new MessageLog();
        var partners = new MessageLog();
        _ = harness.Client.Join(Invoices, invoices.Add);
        var invoiceJoin = await connection.ReadAsync();
        await connection.SendJoinedAsync(Invoices, invoiceJoin.GetProperty("ref").GetString()!, "joined");
        await connection.SendEventAsync(Invoices, 1);
        await connection.SendEventAsync(Invoices, 2);
        _ = harness.Client.Join(Partners, partners.Add);
        var partnerJoin = await connection.ReadAsync();
        await connection.SendJoinedAsync(Partners, partnerJoin.GetProperty("ref").GetString()!, "joined");
        await connection.SendEventAsync(Partners, 1);

        await Wait.UntilAsync(
            () => invoices.Events.Count == 2 && partners.Events.Count == 1,
            "three events before the drop");

        // 4008: the server decided this client reads too slowly.
        await connection.CloseAsync(4008);
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Reconnecting, "the reconnect wait");
        await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");

        var retry = await harness.Server.WaitForConnectionAsync(1);
        await retry.ReadAuthAsync();
        await retry.SendReadyAsync();

        var joins = await ReadJoinsAsync(retry, 2);
        Assert.Equal(2, joins[Invoices].GetProperty("resume_after").GetInt64());
        Assert.Equal(1, joins[Partners].GetProperty("resume_after").GetInt64());

        // The server replays the two events after the resume point; they arrive once, in order.
        await retry.SendJoinedAsync(Invoices, joins[Invoices].GetProperty("ref").GetString()!, "replayed", 0);
        await retry.SendJoinedAsync(Partners, joins[Partners].GetProperty("ref").GetString()!, "replayed", 0);
        await retry.SendEventAsync(Invoices, 3);
        await retry.SendEventAsync(Invoices, 4);

        await Wait.UntilAsync(() => invoices.Events.Count == 4, "the resumed events");
        Assert.Equal([1L, 2, 3, 4], invoices.Seqs);
        Assert.Equal([1L], partners.Seqs);
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);
    }

    [Fact]
    public async Task A_slow_consumer_resume_can_answer_resync_required()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        var subscription = harness.Client.Join(Invoices, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Invoices, join.GetProperty("ref").GetString()!, "joined");
        await connection.SendEventAsync(Invoices, 1);
        await Wait.UntilAsync(() => log.Events.Count == 1, "the first event");

        await connection.CloseAsync(4008);
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Reconnecting, "the reconnect wait");
        await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");
        var retry = await harness.Server.WaitForConnectionAsync(1);
        await retry.ReadAuthAsync();
        await retry.SendReadyAsync();
        var resumed = await retry.ReadAsync();
        Assert.Equal(1, resumed.GetProperty("resume_after").GetInt64());

        await retry.SendJoinedAsync(Invoices, resumed.GetProperty("ref").GetString()!, "resync_required");

        await Wait.UntilAsync(() => log.Snapshot().OfType<RealtimeResyncRequired>().Any(), "the resync signal");
        Assert.Equal(RealtimeResyncReason.ResumeExpired, log.Single<RealtimeResyncRequired>().Reason);
        subscription.AcknowledgeResync();
    }

    [Fact]
    public async Task The_socket_is_treated_as_dead_after_two_missed_heartbeats()
    {
        await using var harness = RealtimeHarness.Create();
        _ = await harness.ConnectAsync(heartbeatSeconds: 5);

        await harness.Clock.AdvanceWhenTimerAsync(TimeSpan.FromSeconds(9), "the heartbeat timer");
        Assert.Equal(1, harness.Server.ConnectionCount);

        await harness.Clock.AdvanceWhenTimerAsync(TimeSpan.FromSeconds(6), "the heartbeat timer");
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Reconnecting, "the heartbeat drop");
        await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");
        var retry = await harness.Server.WaitForConnectionAsync(1);
        await retry.ReadAuthAsync();
        await retry.SendReadyAsync();
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Ready, "the second ready");
        Assert.Contains(harness.Diagnostics, line => line.Contains("heartbeat timeout", StringComparison.Ordinal));
    }

    [Fact]
    public async Task The_server_pings_keep_the_connection_alive()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync(heartbeatSeconds: 5);

        // Past twice the heartbeat, with a ping inside every heartbeat: the socket is alive,
        // so the client must answer by staying (the control frame goes back automatically).
        for (var i = 0; i < 4; i++)
        {
            var before = harness.Client.ReceivedFrameCount;
            await connection.SendPingAsync();
            await Wait.UntilAsync(() => harness.Client.ReceivedFrameCount > before, "the ping to arrive");
            await harness.Clock.AdvanceWhenTimerAsync(TimeSpan.FromSeconds(4), "the heartbeat timer");
        }

        Assert.Equal(1, harness.Server.ConnectionCount);
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);
    }

    [Fact]
    public void The_backoff_is_full_jitter_between_one_and_thirty_seconds()
    {
        var caps = Enumerable.Range(1, 8).Select(RealtimeBackoff.CapFor).ToArray();
        Assert.Equal(
            [
                TimeSpan.FromSeconds(1),
                TimeSpan.FromSeconds(2),
                TimeSpan.FromSeconds(4),
                TimeSpan.FromSeconds(8),
                TimeSpan.FromSeconds(16),
                TimeSpan.FromSeconds(30),
                TimeSpan.FromSeconds(30),
                TimeSpan.FromSeconds(30),
            ],
            caps);

        var backoff = new RealtimeBackoff(new Random(20261029));
        for (var attempt = 1; attempt <= 40; attempt++)
        {
            var delay = backoff.NextDelay(attempt);
            Assert.InRange(delay, TimeSpan.Zero, RealtimeBackoff.CapFor(attempt));
        }

        // The same seed gives the same schedule: the jitter is an injected dependency.
        var first = Enumerable.Range(1, 12).Select(a => new RealtimeBackoff(new Random(4242)).NextDelay(a)).ToArray();
        var second = Enumerable.Range(1, 12).Select(a => new RealtimeBackoff(new Random(4242)).NextDelay(a)).ToArray();
        Assert.Equal(first, second);
    }

    [Fact]
    public async Task Every_room_is_rejoined_after_a_draining_close()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        _ = harness.Client.Join(Invoices, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Invoices, join.GetProperty("ref").GetString()!, "joined");
        await connection.SendEventAsync(Invoices, 7);
        await Wait.UntilAsync(() => log.Events.Count == 1, "the first event");

        await connection.CloseAsync(1001);
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Reconnecting, "the reconnect wait");
        await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");

        var retry = await harness.Server.WaitForConnectionAsync(1);
        await retry.ReadAuthAsync();
        await retry.SendReadyAsync();
        var resumed = await retry.ReadAsync();
        Assert.Equal("join", resumed.GetProperty("type").GetString());
        Assert.Equal(Invoices, resumed.GetProperty("room").GetString());
        Assert.Equal(7, resumed.GetProperty("resume_after").GetInt64());
    }

    private static async Task<Dictionary<string, JsonElement>> ReadJoinsAsync(ScriptedServer.Connection connection, int count)
    {
        var joins = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
        for (var i = 0; i < count; i++)
        {
            var message = await connection.ReadAsync();
            Assert.Equal("join", message.GetProperty("type").GetString());
            joins[message.GetProperty("room").GetString()!] = message;
        }

        return joins;
    }
}
