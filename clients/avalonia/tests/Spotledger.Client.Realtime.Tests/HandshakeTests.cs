using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>
/// The handshake: <c>auth</c> first, nothing else before <c>ready</c>, the contract's endpoint
/// and subprotocol, and the auth window.
/// </summary>
public sealed class HandshakeTests
{
    [Fact]
    public async Task Auth_is_the_first_message_and_nothing_is_sent_before_ready()
    {
        await using var harness = RealtimeHarness.Create();
        var connect = harness.Client.ConnectAsync("session-token-under-test", CancellationToken.None);
        var connection = await harness.Server.WaitForConnectionAsync(0);

        Assert.Equal("session-token-under-test", await connection.ReadAuthAsync());
        await connection.AssertNothingElseSentAsync();
        Assert.Equal(RealtimeConnectionState.Connecting, harness.Client.State);

        await connection.SendReadyAsync();
        await connect.WithTimeoutAsync("the client to become ready");

        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);
        Assert.Equal("0192f3c4-5b6a-7c8d-9e0f-00000000aaaa", harness.Client.UserId);
    }

    [Fact]
    public async Task A_join_requested_before_ready_is_sent_only_after_ready()
    {
        await using var harness = RealtimeHarness.Create();
        var log = new MessageLog();
        using var subscription = harness.Client.Join("site", log.Add);

        var connect = harness.Client.ConnectAsync("session-token", CancellationToken.None);
        var connection = await harness.Server.WaitForConnectionAsync(0);
        await connection.ReadAuthAsync();
        await connection.AssertNothingElseSentAsync();

        await connection.SendReadyAsync();
        await connect.WithTimeoutAsync("the client to become ready");

        var join = await connection.ReadAsync();
        Assert.Equal("join", join.GetProperty("type").GetString());
        Assert.Equal("site", join.GetProperty("room").GetString());
        // A first join has no resume point: the server replays nothing.
        Assert.False(join.TryGetProperty("resume_after", out _));
        Assert.Equal("j1", join.GetProperty("ref").GetString());
    }

    [Fact]
    public async Task The_endpoint_and_subprotocol_are_the_contract_ones()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        Assert.Equal("wss://erp.example.test/api/v1/realtime", harness.Client.Endpoint.ToString());
        Assert.Equal("wss://erp.example.test/api/v1/realtime", connection.Transport.Endpoint?.ToString());
        Assert.Equal("spot.v1", connection.Transport.NegotiatedSubProtocol);
    }

    [Fact]
    public async Task Ready_must_arrive_within_the_auth_window()
    {
        await using var harness = RealtimeHarness.Create();
        _ = harness.Client.ConnectAsync("session-token", CancellationToken.None);
        var first = await harness.Server.WaitForConnectionAsync(0);
        await first.ReadAuthAsync();

        // Nothing from the server: the handshake cannot complete.
        await harness.Clock.AdvanceWhenTimerAsync(TimeSpan.FromSeconds(9), "the auth window");
        Assert.Equal(1, harness.Server.ConnectionCount);

        await harness.Clock.AdvanceWhenTimerAsync(TimeSpan.FromSeconds(2), "the auth window");
        await Wait.UntilAsync(
            () => harness.Client.State == RealtimeConnectionState.Reconnecting,
            "the client to give up on the silent handshake");
        await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");
        await Wait.UntilAsync(
            () => harness.Server.ConnectionCount >= 2,
            "the client to retry after the silent handshake");
        var second = await harness.Server.WaitForConnectionAsync(1);
        Assert.Equal("auth", (await second.ReadAsync()).GetProperty("type").GetString());
    }
}
