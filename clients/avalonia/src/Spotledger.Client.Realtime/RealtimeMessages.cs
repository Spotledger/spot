using System.Text.Json;

namespace Spotledger.Client.Realtime;

/// <summary>A message the client sends. The wire shape is one JSON object per frame.</summary>
public abstract record RealtimeClientMessage
{
    /// <summary>The <c>type</c> discriminator on the wire.</summary>
    public abstract string Type { get; }
}

/// <summary><c>{type: auth, token}</c>: the first message of every connection.</summary>
/// <remarks>
/// The token is redacted everywhere it could be rendered: <see cref="ToString"/> reports
/// only that a token is present, so no log line, diagnostic or exception can leak it.
/// </remarks>
public sealed record RealtimeAuthMessage(string Token) : RealtimeClientMessage
{
    /// <inheritdoc/>
    public override string Type => "auth";

    /// <summary>Presence only — never the token itself.</summary>
    public override string ToString() => "auth(token=***)";
}

/// <summary><c>{type: join, ref, room, resume_after?}</c>.</summary>
public sealed record RealtimeJoinMessage(string Ref, string Room, long? ResumeAfter = null) : RealtimeClientMessage
{
    /// <inheritdoc/>
    public override string Type => "join";
}

/// <summary><c>{type: leave, room}</c>.</summary>
public sealed record RealtimeLeaveMessage(string Room) : RealtimeClientMessage
{
    /// <inheritdoc/>
    public override string Type => "leave";
}

/// <summary><c>{type: ping}</c>.</summary>
public sealed record RealtimePingMessage : RealtimeClientMessage
{
    /// <inheritdoc/>
    public override string Type => "ping";
}

/// <summary>A message the server sends. Unknown types implement <see cref="RealtimeUnknownServerMessage"/>.</summary>
public interface IRealtimeServerMessage
{
    /// <summary>The <c>type</c> discriminator on the wire.</summary>
    string Type { get; }
}

/// <summary><c>{type: ready, user, heartbeat_seconds}</c>: <c>auth</c> succeeded.</summary>
public sealed record RealtimeReady(string User, int HeartbeatSeconds) : IRealtimeServerMessage
{
    /// <inheritdoc/>
    public string Type => "ready";
}

/// <summary><c>{type: joined, ref, room, outcome, replayed?}</c>.</summary>
public sealed record RealtimeJoined(
    string Ref,
    string Room,
    RealtimeJoinOutcome Outcome,
    int? Replayed = null) : IRealtimeServerMessage
{
    /// <inheritdoc/>
    public string Type => "joined";
}

/// <summary><c>{type: pong}</c>.</summary>
public sealed record RealtimePong : IRealtimeServerMessage
{
    /// <inheritdoc/>
    public string Type => "pong";
}

/// <summary>A server message type this version does not know: counted and ignored.</summary>
public sealed record RealtimeUnknownServerMessage(string Type, JsonElement Body) : IRealtimeServerMessage;

/// <summary>Something a subscriber receives for one room.</summary>
/// <param name="Room">The room it belongs to, exactly as joined.</param>
public abstract record RealtimeRoomMessage(string Room);

/// <summary>
/// One <c>event</c> for a room: a hint, not data. Refetch what you show; never treat it as
/// the document itself.
/// </summary>
public sealed record RealtimeEvent(
    string Room,
    long Seq,
    RealtimeEventKind Kind,
    string RawKind,
    JsonElement Payload,
    DateTimeOffset PublishedAt) : RealtimeRoomMessage(Room), IRealtimeServerMessage
{
    /// <inheritdoc/>
    public string Type => "event";
}

/// <summary>
/// The room's event stream cannot be trusted from here: refetch the room's data. Raised for
/// a <c>seq</c> gap and for a <c>joined</c> with <c>outcome = resync_required</c>. Events for
/// the room are withheld until the subscription acknowledges the resync.
/// </summary>
public sealed record RealtimeResyncRequired(
    string Room,
    RealtimeResyncReason Reason,
    long LastSeq) : RealtimeRoomMessage(Room);

/// <summary><c>{type: join_error, ref, room, error}</c>: the join was refused.</summary>
public sealed record RealtimeJoinError(
    string Room,
    string Ref,
    string Code,
    string Message) : RealtimeRoomMessage(Room), IRealtimeServerMessage
{
    /// <inheritdoc/>
    public string Type => "join_error";
}
