namespace Spotledger.Client.Realtime;

/// <summary>
/// Constants from <c>contracts/native-api/realtime.md</c> v0.1. They are the wire
/// contract, so they live in one place and every rule names the value it enforces.
/// </summary>
public static class RealtimeProtocol
{
    /// <summary>The only subprotocol the server speaks; an upgrade without it is refused.</summary>
    public const string SubProtocol = "spot.v1";

    /// <summary>The upgrade path on the site's host name.</summary>
    public const string EndpointPath = "/api/v1/realtime";

    /// <summary>One frame is one UTF-8 JSON text message; anything larger is a protocol fault.</summary>
    public const int MaxFrameBytes = 64 * 1024;

    /// <summary>The server refuses a join that would put a client over this many rooms.</summary>
    public const int MaxRooms = 200;

    /// <summary>The first message must carry the session token within this window.</summary>
    public static readonly TimeSpan AuthWindow = TimeSpan.FromSeconds(10);

    /// <summary>Backoff starts here and doubles per attempt.</summary>
    public static readonly TimeSpan BackoffBase = TimeSpan.FromSeconds(1);

    /// <summary>Backoff never exceeds this, and full jitter stays inside the cap.</summary>
    public static readonly TimeSpan BackoffCap = TimeSpan.FromSeconds(30);

    /// <summary>A connection that has heard nothing for twice the heartbeat is dead.</summary>
    public const int HeartbeatMissMultiplier = 2;

    /// <summary>The default <c>heartbeat_seconds</c> the server sends in <c>ready</c>.</summary>
    public const int DefaultHeartbeatSeconds = 25;

    /// <summary>The site root, so callers can build the endpoint without string surgery.</summary>
    public static Uri EndpointFor(Uri site)
    {
        ArgumentNullException.ThrowIfNull(site);
        var builder = new UriBuilder(site)
        {
            Scheme = string.Equals(site.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase) ? "wss" : "ws",
            Path = EndpointPath,
            Query = string.Empty,
        };
        return builder.Uri;
    }
}

/// <summary>Close codes from the contract's table, with the client action each one means.</summary>
public static class RealtimeCloseCodes
{
    /// <summary>Normal close: do nothing.</summary>
    public const int Normal = 1000;

    /// <summary>Server shutting down or draining: reconnect after backoff and resume.</summary>
    public const int GoingAway = 1001;

    /// <summary>Binary frame sent (a client bug): stop, do not loop.</summary>
    public const int UnsupportedData = 1003;

    /// <summary>Protocol violation (a client bug): stop, do not loop.</summary>
    public const int ProtocolError = 1008;

    /// <summary>Frame too large (a client bug): stop, do not loop.</summary>
    public const int MessageTooBig = 1009;

    /// <summary>Authentication failed or timed out: sign in again, then reconnect.</summary>
    public const int AuthenticationFailed = 4001;

    /// <summary>Slow consumer: reconnect and resume; expect <c>resync_required</c>.</summary>
    public const int SlowConsumer = 4008;

    /// <summary>Session expired or revoked: sign in again.</summary>
    public const int SessionExpired = 4009;
}

/// <summary>The <c>kind</c> values of an <c>event</c>, plus a forward-compatible escape.</summary>
public enum RealtimeEventKind
{
    /// <summary>An unknown kind: the server added one, so read <see cref="RealtimeEvent.RawKind"/>.</summary>
    Unknown = 0,
    DocCreated = 1,
    DocUpdated = 2,
    DocDeleted = 3,
    DocPosted = 4,
    DocCancelled = 5,
    DocRenamed = 6,
    Notification = 7,
}

/// <summary>The <c>outcome</c> of a <c>joined</c> reply.</summary>
public enum RealtimeJoinOutcome
{
    /// <summary>No resume was requested.</summary>
    Joined = 0,
    /// <summary>The events after <c>resume_after</c> follow immediately, in order.</summary>
    Replayed = 1,
    /// <summary>The server no longer holds the events after <c>resume_after</c>: refetch.</summary>
    ResyncRequired = 2,
}

/// <summary>Why a subscriber must refetch the room it shows.</summary>
public enum RealtimeResyncReason
{
    /// <summary>A <c>seq</c> gap: messages were lost between two events the client received.</summary>
    Gap = 0,
    /// <summary>The server answered a resume with <c>resync_required</c>.</summary>
    ResumeExpired = 1,
}

/// <summary>Why a room's <c>join</c> was refused, from the contract's error registry.</summary>
public static class RealtimeJoinErrorCodes
{
    /// <summary>The caller may not read the room.</summary>
    public const string PermissionDenied = "permission.denied";

    /// <summary>No such room on this site.</summary>
    public const string UnknownRoom = "realtime.unknown_room";

    /// <summary>The client would hold more than 200 rooms.</summary>
    public const string TooManyRooms = "realtime.too_many_rooms";
}

/// <summary>The lifecycle of one <see cref="RealtimeClient"/>.</summary>
public enum RealtimeConnectionState
{
    /// <summary>No connection is held (before the first connect, after a normal close or a fatal fault).</summary>
    Closed = 0,
    /// <summary>Connecting, sending <c>auth</c>, and waiting for <c>ready</c>.</summary>
    Connecting = 1,
    /// <summary>Signed in and ready; rooms are joined.</summary>
    Ready = 2,
    /// <summary>Waiting out the backoff before the next attempt.</summary>
    Reconnecting = 3,
    /// <summary>The session is gone (4001/4009): the shell must run its re-sign-in flow.</summary>
    SignedOut = 4,
}
