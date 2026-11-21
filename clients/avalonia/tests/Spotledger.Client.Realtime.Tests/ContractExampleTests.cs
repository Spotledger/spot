using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>Every example in <c>openapi.yaml</c> round-trips through the codec, and the event example drives the client.</summary>
public sealed class ContractExampleTests
{
    [Fact]
    public void The_contract_carries_the_examples_this_suite_covers()
    {
        // A contract change that adds an example must be seen here, not silently skipped.
        Assert.Equal(4, ContractExamples.ClientMessages.Count);
        Assert.Equal(5, ContractExamples.ServerMessages.Count);
    }

    [Fact]
    public void Every_client_example_round_trips()
    {
        foreach (var json in ContractExamples.ClientMessages)
        {
            var message = RealtimeMessageCodec.ParseClientMessage(Encoding.UTF8.GetBytes(json));
            Assert.Equal(message.GetType(), RealtimeMessageCodec.ParseClientMessage(RealtimeMessageCodec.Serialize(message)).GetType());

            var original = JsonNode.Parse(json);
            var roundTripped = JsonNode.Parse(Encoding.UTF8.GetString(RealtimeMessageCodec.Serialize(message)));
            Assert.True(JsonNode.DeepEquals(original, roundTripped), $"Round trip changed the message: {json} -> {roundTripped}");
        }
    }

    [Fact]
    public void Every_server_example_round_trips()
    {
        foreach (var json in ContractExamples.ServerMessages)
        {
            var message = RealtimeMessageCodec.ParseServerMessage(Encoding.UTF8.GetBytes(json));
            var serialized = Encoding.UTF8.GetString(RealtimeMessageCodec.SerializeServer(message));

            Assert.True(
                JsonNode.DeepEquals(Canonical(json), Canonical(serialized)),
                $"Round trip changed the message: {json} -> {serialized}");
        }
    }

    [Fact]
    public void The_examples_parse_into_the_types_the_contract_names()
    {
        var parsed = ContractExamples.ServerMessages
            .Select(json => RealtimeMessageCodec.ParseServerMessage(Encoding.UTF8.GetBytes(json)))
            .ToArray();

        var ready = Assert.IsType<RealtimeReady>(parsed[0]);
        Assert.Equal(25, ready.HeartbeatSeconds);

        var joined = Assert.IsType<RealtimeJoined>(parsed[1]);
        Assert.Equal(RealtimeJoinOutcome.Replayed, joined.Outcome);
        Assert.Equal(2, joined.Replayed);

        var error = Assert.IsType<RealtimeJoinError>(parsed[2]);
        Assert.Equal(RealtimeJoinErrorCodes.PermissionDenied, error.Code);

        var @event = Assert.IsType<RealtimeEvent>(parsed[3]);
        Assert.Equal(14, @event.Seq);
        Assert.Equal(RealtimeEventKind.DocUpdated, @event.Kind);
        Assert.Equal("sales_invoice", @event.Payload.GetProperty("doctype").GetString());
        Assert.Equal(new DateTimeOffset(2026, 9, 29, 10, 15, 2, 120, TimeSpan.Zero), @event.PublishedAt);

        Assert.IsType<RealtimePong>(parsed[4]);
    }

    [Fact]
    public async Task The_contracts_event_example_is_delivered_verbatim()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync();
        var log = new MessageLog();
        const string Room = "doc:sales_invoice:0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f";
        _ = harness.Client.Join(Room, log.Add);
        var join = await connection.ReadAsync();
        await connection.SendJoinedAsync(Room, join.GetProperty("ref").GetString()!, "joined");

        var eventExample = ContractExamples.ServerMessages[3];
        await connection.SendRawAsync(eventExample);

        await Wait.UntilAsync(() => log.Events.Count == 1, "the contract's event example");
        var @event = log.Events[0];
        Assert.Equal(14, @event.Seq);
        Assert.Equal(Room, @event.Room);
        Assert.Equal("doc_updated", @event.RawKind);
        Assert.Equal(4, @event.Payload.GetProperty("version").GetInt32());
        Assert.Equal(new DateTimeOffset(2026, 9, 29, 10, 15, 2, 120, TimeSpan.Zero), @event.PublishedAt);
    }

    /// <summary>Compares timestamps by instant, not by how the fraction was written.</summary>
    private static JsonNode? Canonical(string json)
    {
        var node = JsonNode.Parse(json);
        if (node is JsonObject obj && obj["published_at"] is JsonValue value && value.TryGetValue<string>(out var text))
        {
            obj["published_at"] = DateTimeOffset.Parse(
                text,
                System.Globalization.CultureInfo.InvariantCulture,
                System.Globalization.DateTimeStyles.AdjustToUniversal).UtcTicks;
        }

        return node;
    }
}
