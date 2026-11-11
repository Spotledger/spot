using System.Text;
using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>Frames the contract does not allow: what is ignored, what is counted, what stops the client.</summary>
public sealed class ProtocolRobustnessTests
{
    private const string Room = "doctype:sales_invoice";

    [Fact]
    public async Task A_duplicate_ready_is_ignored_and_counted()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        await connection.SendReadyAsync();

        await Wait.UntilAsync(() => harness.Client.DuplicateReadyCount == 1, "the duplicate ready to be counted");
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);
        Assert.Equal(1, harness.Server.ConnectionCount);

        // The connection still works: a join is answered as usual.
        var log = new MessageLog();
        _ = harness.Client.Join(Room, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Room, join.GetProperty("ref").GetString()!, "joined");
        await connection.SendEventAsync(Room, 1);
        await Wait.UntilAsync(() => log.Events.Count == 1, "an event after the duplicate ready");
    }

    [Fact]
    public async Task An_unknown_message_type_is_ignored_and_counted()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        _ = harness.Client.Join(Room, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Room, join.GetProperty("ref").GetString()!, "joined");

        // The server may add message types; an old client must not break on one.
        await connection.SendRawAsync("{\"type\":\"presence\",\"room\":\"doctype:sales_invoice\",\"users\":[]}");

        await Wait.UntilAsync(() => harness.Client.UnknownServerMessageCount == 1, "the unknown type to be counted");
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);

        await connection.SendEventAsync(Room, 1);
        await Wait.UntilAsync(() => log.Events.Count == 1, "an event after the unknown message");
    }

    [Fact]
    public async Task A_binary_frame_from_the_server_closes_the_connection()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        await connection.SendBinaryAsync();

        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Closed, "the closed state");
        // Deliberately no timer pending: the client stopped, so this proves nothing is scheduled.
        harness.Clock.Advance(TimeSpan.FromMinutes(5));
        Assert.Equal(1, harness.Server.ConnectionCount);
        Assert.Contains(harness.Diagnostics, line => line.Contains("binary", StringComparison.Ordinal));
    }

    [Fact]
    public async Task An_oversized_frame_is_refused_before_parsing()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        // A valid JSON message that is over the contract's 64 KiB: it must never be parsed.
        var padding = new string('x', RealtimeProtocol.MaxFrameBytes + 1024);
        var oversized = Encoding.UTF8.GetBytes($"{{\"type\":\"pong\",\"pad\":\"{padding}\"}}");
        await connection.SendRawAsync(oversized);

        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Closed, "the closed state");
        Assert.Equal(0, harness.Client.UnknownServerMessageCount);
        Assert.Equal(1, harness.Server.ConnectionCount);
        Assert.Contains(harness.Diagnostics, line => line.Contains("64 KiB", StringComparison.Ordinal));
    }

    [Fact]
    public async Task Invalid_json_is_a_protocol_fault_and_stops_the_client()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();

        await connection.SendRawAsync("{ this is not json");

        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Closed, "the closed state");
        // Deliberately no timer pending: the client stopped, so this proves nothing is scheduled.
        harness.Clock.Advance(TimeSpan.FromMinutes(5));
        Assert.Equal(1, harness.Server.ConnectionCount);
        Assert.Contains(harness.Diagnostics, line => line.Contains("protocol fault", StringComparison.Ordinal));
    }

    [Fact]
    public async Task An_event_for_a_room_we_left_is_dropped_without_a_fault()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        var subscription = harness.Client.Join(Room, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Room, join.GetProperty("ref").GetString()!, "joined");
        subscription.Dispose();
        await connection.ReadAsync(); // the leave

        await connection.SendEventAsync(Room, 1);

        await Wait.UntilAsync(() => harness.Client.DroppedMessageCount >= 1, "the stray event to be dropped");
        Assert.Empty(log.Snapshot());
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);
    }
}
