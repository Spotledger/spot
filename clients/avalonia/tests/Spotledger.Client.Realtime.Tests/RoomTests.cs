using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>Rooms: one wire join per room, the client-side limit, join errors, and the seq rules.</summary>
public sealed class RoomTests
{
    private const string DocRoom = "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f";

    [Fact]
    public async Task One_join_and_one_leave_per_room_however_many_subscribers()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var first = new MessageLog();
        var second = new MessageLog();

        var a = harness.Client.Join(DocRoom, first.Add);
        var join = await connection.ReadAsync();
        Assert.Equal("join", join.GetProperty("type").GetString());
        await connection.SendJoinedAsync(DocRoom, join.GetProperty("ref").GetString()!, "joined");

        var b = harness.Client.Join(DocRoom, second.Add);
        await connection.AssertNoMessageAsync("a second subscriber reuses the existing join");
        Assert.Empty(first.Snapshot());
        Assert.Empty(second.Snapshot());

        a.Dispose();
        await connection.AssertNoMessageAsync("one subscriber leaving keeps the room on the wire");

        b.Dispose();
        var leave = await connection.ReadAsync();
        Assert.Equal("leave", leave.GetProperty("type").GetString());
        Assert.Equal(DocRoom, leave.GetProperty("room").GetString());
        Assert.Equal(0, harness.Client.RoomCount);
    }

    [Fact]
    public async Task The_room_limit_is_enforced_locally_before_the_server_refuses()
    {
        await using var harness = RealtimeHarness.Create();
        await harness.ConnectAsync();
        var log = new MessageLog();
        for (var i = 0; i < RealtimeProtocol.MaxRooms; i++)
        {
            harness.Client.Join($"doctype:table_{i}", log.Add);
        }

        var refused = Assert.Throws<RealtimeRoomLimitException>(() => harness.Client.Join("doctype:one_too_many", log.Add));
        Assert.Contains("200", refused.Message, StringComparison.Ordinal);
        Assert.Contains("doctype:one_too_many", refused.Message, StringComparison.Ordinal);
        Assert.Equal(RealtimeProtocol.MaxRooms, harness.Client.RoomCount);
    }

    [Fact]
    public async Task A_join_error_reaches_the_subscriber_and_the_connection_survives()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        _ = harness.Client.Join("doctype:salary_slip", log.Add);

        var join = await connection.ReadAsync();
        await connection.SendJoinErrorAsync("doctype:salary_slip", join.GetProperty("ref").GetString()!, "permission.denied", "You cannot see salary slips.");

        await Wait.UntilAsync(() => log.Count == 1, "the join error");
        var error = log.Single<RealtimeJoinError>();
        Assert.Equal("permission.denied", error.Code);
        Assert.Equal("You cannot see salary slips.", error.Message);
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);
    }

    [Fact]
    public async Task A_join_error_of_an_unknown_room_is_delivered_too()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        _ = harness.Client.Join("doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-00000000dead", log.Add);

        var join = await connection.ReadAsync();
        await connection.SendJoinErrorAsync(
            "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-00000000dead",
            join.GetProperty("ref").GetString()!,
            "realtime.unknown_room");

        await Wait.UntilAsync(() => log.Count == 1, "the join error");
        Assert.Equal("realtime.unknown_room", log.Single<RealtimeJoinError>().Code);
    }

    [Fact]
    public async Task Events_are_delivered_in_order_exactly_once()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        _ = harness.Client.Join(DocRoom, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(DocRoom, join.GetProperty("ref").GetString()!, "joined");

        for (var seq = 1; seq <= 5; seq++)
        {
            await connection.SendEventAsync(DocRoom, seq);
        }

        await Wait.UntilAsync(() => log.Events.Count == 5, "five events");
        Assert.Equal([1L, 2, 3, 4, 5], log.Seqs);
        Assert.All(log.Events, e => Assert.Equal(DocRoom, e.Room));
        Assert.Equal(RealtimeEventKind.DocUpdated, log.Events[0].Kind);
        Assert.Equal(4, log.Events[0].Payload.GetProperty("version").GetInt32());
    }

    [Fact]
    public async Task A_seq_gap_raises_resync_required_and_withholds_later_events_until_acknowledged()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        var subscription = harness.Client.Join(DocRoom, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(DocRoom, join.GetProperty("ref").GetString()!, "joined");

        await connection.SendEventAsync(DocRoom, 1);
        await Wait.UntilAsync(() => log.Events.Count == 1, "the first event");

        // seq 3 after seq 1: seq 2 was lost, so 3 must not be delivered as if nothing happened.
        await connection.SendEventAsync(DocRoom, 3);
        await Wait.UntilAsync(() => log.Snapshot().OfType<RealtimeResyncRequired>().Any(), "the resync signal");

        var resync = log.Single<RealtimeResyncRequired>();
        Assert.Equal(RealtimeResyncReason.Gap, resync.Reason);
        Assert.Equal(1, resync.LastSeq);
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);

        // While the room awaits its refetch, later events are withheld.
        await connection.SendEventAsync(DocRoom, 4);
        await Wait.UntilAsync(() => harness.Client.DroppedMessageCount >= 2, "the withheld event");

        subscription.AcknowledgeResync();
        await connection.SendEventAsync(DocRoom, 5);

        await Wait.UntilAsync(() => log.Events.Count == 2, "the first event after the refetch");
        Assert.Equal([1L, 5], log.Seqs);
    }

    [Fact]
    public async Task Joined_with_resync_required_raises_the_same_signal()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        var subscription = harness.Client.Join(DocRoom, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(DocRoom, join.GetProperty("ref").GetString()!, "resync_required");

        await Wait.UntilAsync(() => log.Count == 1, "the resync signal");
        var resync = log.Single<RealtimeResyncRequired>();
        Assert.Equal(RealtimeResyncReason.ResumeExpired, resync.Reason);

        // The room is in, but its events stay withheld until the subscriber refetched.
        await connection.SendEventAsync(DocRoom, 9);
        await Wait.UntilAsync(() => harness.Client.DroppedMessageCount >= 1, "the withheld event");

        subscription.AcknowledgeResync();
        await connection.SendEventAsync(DocRoom, 10);
        await Wait.UntilAsync(() => log.Events.Count == 1, "the first event after the refetch");
        Assert.Equal(10, log.Events[0].Seq);
    }
}
