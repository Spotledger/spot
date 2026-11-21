using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>Backpressure: a slow handler drops the connection, a throwing handler does not.</summary>
public sealed class BackpressureTests
{
    private const string Room = "doctype:sales_invoice";

    [Fact]
    public async Task A_handler_slower_than_the_stream_makes_the_client_drop_the_connection()
    {
        await using var harness = RealtimeHarness.Create(handlerQueueCapacity: 2);
        var connection = await harness.ConnectAsync();
        using var gate = new SemaphoreSlim(0, 1);
        var delivered = new MessageLog();
        _ = harness.Client.Join(Room, message =>
        {
            delivered.Add(message);
            // The handler stops consuming: the queue fills and the client must not grow it.
            gate.Wait(TimeSpan.FromSeconds(10));
        });
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Room, join.GetProperty("ref").GetString()!, "joined");

        try
        {
            for (var seq = 1; seq <= 8; seq++)
            {
                await connection.SendEventAsync(Room, seq);
            }

            await Wait.UntilAsync(
                () => harness.Client.State == RealtimeConnectionState.Reconnecting,
                "the client to drop a slow consumer and reconnect");

            await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");
            var retry = await harness.Server.WaitForConnectionAsync(1);
            await retry.ReadAuthAsync();
            await retry.SendReadyAsync();
            await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Ready, "the resumed connection");
            Assert.Contains(harness.Diagnostics, line => line.Contains("slow consumer", StringComparison.Ordinal));
        }
        finally
        {
            gate.Release();
        }
    }

    [Fact]
    public async Task A_handler_exception_does_not_kill_the_receive_loop()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var received = new MessageLog();
        _ = harness.Client.Join(Room, message =>
        {
            if (message is RealtimeEvent { Seq: 1 })
            {
                throw new InvalidOperationException("the subscriber's own bug");
            }

            received.Add(message);
        });
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Room, join.GetProperty("ref").GetString()!, "joined");

        await connection.SendEventAsync(Room, 1);
        await connection.SendEventAsync(Room, 2);

        await Wait.UntilAsync(() => received.Events.Count == 1, "the second event after a handler threw");
        Assert.Equal(2, received.Events[0].Seq);
        Assert.Equal(1, harness.Client.HandlerFaultCount);
        Assert.Equal(RealtimeConnectionState.Ready, harness.Client.State);
    }

    [Fact]
    public async Task Handlers_run_in_order_on_one_room()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        _ = harness.Client.Join(Room, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Room, join.GetProperty("ref").GetString()!, "joined");

        for (var seq = 1; seq <= 25; seq++)
        {
            await connection.SendEventAsync(Room, seq);
        }

        await Wait.UntilAsync(() => log.Events.Count == 25, "twenty-five events");
        Assert.Equal(Enumerable.Range(1, 25).Select(value => (long)value), log.Seqs);
    }
}
