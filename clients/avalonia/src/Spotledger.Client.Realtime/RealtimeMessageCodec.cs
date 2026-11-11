using System.Buffers;
using System.Globalization;
using System.Text.Json;

namespace Spotledger.Client.Realtime;

/// <summary>
/// The wire format of <c>realtime.md</c> v0.1, written and read by hand: one UTF-8 JSON
/// object per frame, snake_case keys, no reflection. Hand-writing it keeps the browser head
/// trim-safe and lets <see cref="RealtimeAuthMessage.ToString"/> redact the token.
/// </summary>
public static class RealtimeMessageCodec
{
    private const string TimestampFormat = "yyyy-MM-dd'T'HH:mm:ss.FFFFFFF'Z'";

    /// <summary>Serialises a client message to one UTF-8 JSON frame.</summary>
    /// <param name="message">The message to send.</param>
    public static byte[] Serialize(RealtimeClientMessage message)
    {
        ArgumentNullException.ThrowIfNull(message);
        return Write(writer =>
        {
            switch (message)
            {
                case RealtimeAuthMessage auth:
                    writer.WriteString("type", "auth");
                    writer.WriteString("token", auth.Token);
                    break;
                case RealtimeJoinMessage join:
                    writer.WriteString("type", "join");
                    writer.WriteString("ref", join.Ref);
                    writer.WriteString("room", join.Room);
                    if (join.ResumeAfter is { } resumeAfter)
                    {
                        writer.WriteNumber("resume_after", resumeAfter);
                    }

                    break;
                case RealtimeLeaveMessage leave:
                    writer.WriteString("type", "leave");
                    writer.WriteString("room", leave.Room);
                    break;
                case RealtimePingMessage:
                    writer.WriteString("type", "ping");
                    break;
                default:
                    throw new ArgumentException($"Unknown client message type '{message.GetType().Name}'.", nameof(message));
            }
        });
    }

    /// <summary>Parses one UTF-8 JSON frame the client sends, so a test (or a fake server) can read it.</summary>
    /// <param name="utf8">The frame payload.</param>
    /// <exception cref="RealtimeProtocolException">The frame is not a contract message.</exception>
    public static RealtimeClientMessage ParseClientMessage(ReadOnlyMemory<byte> utf8)
    {
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(utf8);
        }
        catch (JsonException ex)
        {
            throw new RealtimeProtocolException("The client sent a frame that is not valid JSON.", ex);
        }

        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                throw new RealtimeProtocolException("The client sent a frame that is not a JSON object.");
            }

            var type = RequireString(root, "type");
            switch (type)
            {
                case "auth":
                    return new RealtimeAuthMessage(RequireString(root, "token"));
                case "join":
                    long? resumeAfter = root.TryGetProperty("resume_after", out var resume) && resume.TryGetInt64(out var seq)
                        ? seq
                        : null;
                    return new RealtimeJoinMessage(RequireString(root, "ref"), RequireString(root, "room"), resumeAfter);
                case "leave":
                    return new RealtimeLeaveMessage(RequireString(root, "room"));
                case "ping":
                    return new RealtimePingMessage();
                default:
                    throw new RealtimeProtocolException($"Unknown client message type '{type}'.");
            }
        }
    }

    /// <summary>Serialises a server message, so a test can round-trip a contract example.</summary>
    /// <param name="message">The message to write.</param>
    public static byte[] SerializeServer(IRealtimeServerMessage message)
    {
        ArgumentNullException.ThrowIfNull(message);
        return Write(writer =>
        {
            switch (message)
            {
                case RealtimeReady ready:
                    writer.WriteString("type", "ready");
                    writer.WriteString("user", ready.User);
                    writer.WriteNumber("heartbeat_seconds", ready.HeartbeatSeconds);
                    break;
                case RealtimeJoined joined:
                    writer.WriteString("type", "joined");
                    writer.WriteString("ref", joined.Ref);
                    writer.WriteString("room", joined.Room);
                    writer.WriteString("outcome", OutcomeToWire(joined.Outcome));
                    if (joined.Replayed is { } replayed)
                    {
                        writer.WriteNumber("replayed", replayed);
                    }

                    break;
                case RealtimeJoinError error:
                    writer.WriteString("type", "join_error");
                    writer.WriteString("ref", error.Ref);
                    writer.WriteString("room", error.Room);
                    writer.WriteStartObject("error");
                    writer.WriteString("code", error.Code);
                    writer.WriteString("message", error.Message);
                    writer.WriteEndObject();
                    break;
                case RealtimeEvent @event:
                    writer.WriteString("type", "event");
                    writer.WriteString("room", @event.Room);
                    writer.WriteNumber("seq", @event.Seq);
                    writer.WriteString("kind", @event.RawKind);
                    writer.WritePropertyName("payload");
                    @event.Payload.WriteTo(writer);
                    writer.WriteString("published_at", FormatTimestamp(@event.PublishedAt));
                    break;
                case RealtimePong:
                    writer.WriteString("type", "pong");
                    break;
                case RealtimeUnknownServerMessage unknown:
                    writer.WriteString("type", unknown.Type);
                    foreach (var property in unknown.Body.EnumerateObject())
                    {
                        if (string.Equals(property.Name, "type", StringComparison.Ordinal))
                        {
                            continue;
                        }

                        property.WriteTo(writer);
                    }

                    break;
                default:
                    throw new ArgumentException($"Unknown server message type '{message.GetType().Name}'.", nameof(message));
            }
        });
    }

    /// <summary>Parses one UTF-8 JSON frame from the server.</summary>
    /// <param name="utf8">The frame payload.</param>
    /// <exception cref="RealtimeProtocolException">The frame is not a contract message.</exception>
    public static IRealtimeServerMessage ParseServerMessage(ReadOnlyMemory<byte> utf8)
    {
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(utf8);
        }
        catch (JsonException ex)
        {
            throw new RealtimeProtocolException("The server sent a frame that is not valid JSON.", ex);
        }

        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                throw new RealtimeProtocolException("The server sent a frame that is not a JSON object.");
            }

            var type = RequireString(root, "type");
            switch (type)
            {
                case "ready":
                    var heartbeat = root.TryGetProperty("heartbeat_seconds", out var hb) && hb.TryGetInt32(out var seconds)
                        ? seconds
                        : RealtimeProtocol.DefaultHeartbeatSeconds;
                    return new RealtimeReady(RequireString(root, "user"), heartbeat);
                case "joined":
                    return new RealtimeJoined(
                        RequireString(root, "ref"),
                        RequireString(root, "room"),
                        ParseOutcome(RequireString(root, "outcome")),
                        root.TryGetProperty("replayed", out var replayed) && replayed.TryGetInt32(out var count) ? count : null);
                case "join_error":
                    var error = root.TryGetProperty("error", out var envelope) && envelope.ValueKind == JsonValueKind.Object
                        ? envelope
                        : throw new RealtimeProtocolException("A join_error frame has no error object.");
                    return new RealtimeJoinError(
                        RequireString(root, "room"),
                        RequireString(root, "ref"),
                        RequireString(error, "code"),
                        RequireString(error, "message"));
                case "event":
                    if (!root.TryGetProperty("seq", out var seqElement) || !seqElement.TryGetInt64(out var seq) || seq < 1)
                    {
                        throw new RealtimeProtocolException("An event frame has no positive integer seq.");
                    }

                    var kindText = RequireString(root, "kind");
                    var payload = root.TryGetProperty("payload", out var payloadElement)
                        ? payloadElement.Clone()
                        : throw new RealtimeProtocolException("An event frame has no payload.");
                    return new RealtimeEvent(
                        RequireString(root, "room"),
                        seq,
                        ParseKind(kindText),
                        kindText,
                        payload,
                        ParseTimestamp(RequireString(root, "published_at")));
                case "pong":
                    return new RealtimePong();
                default:
                    // A type this version does not know is not an error: the server may add
                    // message types. It is counted and ignored by the client.
                    return new RealtimeUnknownServerMessage(type, root.Clone());
            }
        }
    }

    /// <summary>The wire name of an event kind; the inverse of <see cref="ParseKind"/>.</summary>
    /// <param name="kind">A known kind.</param>
    public static string KindToWire(RealtimeEventKind kind) => kind switch
    {
        RealtimeEventKind.DocCreated => "doc_created",
        RealtimeEventKind.DocUpdated => "doc_updated",
        RealtimeEventKind.DocDeleted => "doc_deleted",
        RealtimeEventKind.DocPosted => "doc_posted",
        RealtimeEventKind.DocCancelled => "doc_cancelled",
        RealtimeEventKind.DocRenamed => "doc_renamed",
        RealtimeEventKind.Notification => "notification",
        _ => "unknown",
    };

    /// <summary>The wire name of a join outcome; the inverse of <see cref="ParseOutcome"/>.</summary>
    /// <param name="outcome">A known outcome.</param>
    public static string OutcomeToWire(RealtimeJoinOutcome outcome) => outcome switch
    {
        RealtimeJoinOutcome.Joined => "joined",
        RealtimeJoinOutcome.Replayed => "replayed",
        RealtimeJoinOutcome.ResyncRequired => "resync_required",
        _ => throw new ArgumentOutOfRangeException(nameof(outcome), outcome, "Unknown join outcome."),
    };

    private static RealtimeEventKind ParseKind(string kind) => kind switch
    {
        "doc_created" => RealtimeEventKind.DocCreated,
        "doc_updated" => RealtimeEventKind.DocUpdated,
        "doc_deleted" => RealtimeEventKind.DocDeleted,
        "doc_posted" => RealtimeEventKind.DocPosted,
        "doc_cancelled" => RealtimeEventKind.DocCancelled,
        "doc_renamed" => RealtimeEventKind.DocRenamed,
        "notification" => RealtimeEventKind.Notification,
        _ => RealtimeEventKind.Unknown,
    };

    private static RealtimeJoinOutcome ParseOutcome(string outcome) => outcome switch
    {
        "joined" => RealtimeJoinOutcome.Joined,
        "replayed" => RealtimeJoinOutcome.Replayed,
        "resync_required" => RealtimeJoinOutcome.ResyncRequired,
        _ => throw new RealtimeProtocolException($"Unknown join outcome '{outcome}'."),
    };

    private static string RequireString(JsonElement element, string name)
    {
        if (!element.TryGetProperty(name, out var value) || value.ValueKind != JsonValueKind.String)
        {
            throw new RealtimeProtocolException($"A server message is missing the string property '{name}'.");
        }

        return value.GetString() ?? throw new RealtimeProtocolException($"The property '{name}' is null.");
    }

    private static DateTimeOffset ParseTimestamp(string value) =>
        DateTimeOffset.TryParse(
            value,
            CultureInfo.InvariantCulture,
            DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal,
            out var parsed)
            ? parsed
            : throw new RealtimeProtocolException("An event frame has a published_at that is not an RFC 3339 timestamp.");

    private static string FormatTimestamp(DateTimeOffset value) =>
        value.ToUniversalTime().ToString(TimestampFormat, CultureInfo.InvariantCulture);

    private static byte[] Write(Action<Utf8JsonWriter> body)
    {
        var buffer = new ArrayBufferWriter<byte>(256);
        using (var writer = new Utf8JsonWriter(buffer, new JsonWriterOptions { Indented = false }))
        {
            writer.WriteStartObject();
            body(writer);
            writer.WriteEndObject();
        }

        return buffer.WrittenSpan.ToArray();
    }
}
