namespace Spotledger.Client.Realtime;

/// <summary>
/// A frame from the server did not match <c>contracts/native-api/realtime.md</c>. The client
/// treats it as a fault in the peer, logs one redacted diagnostic and stops: retrying a
/// server that speaks another dialect just loops.
/// </summary>
public sealed class RealtimeProtocolException : Exception
{
    /// <summary>Creates the exception with a message that contains no frame content.</summary>
    /// <param name="message">What was wrong, in words.</param>
    public RealtimeProtocolException(string message)
        : base(message)
    {
    }

    /// <summary>Creates the exception, keeping the parser's cause (never the frame body).</summary>
    /// <param name="message">What was wrong, in words.</param>
    /// <param name="innerException">The parsing failure.</param>
    public RealtimeProtocolException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}

/// <summary>
/// A connection attempt failed before <c>ready</c>. The message never contains the session
/// token, a URL with credentials, or a raw address.
/// </summary>
public sealed class RealtimeConnectionException : Exception
{
    /// <summary>Creates the exception.</summary>
    /// <param name="message">What failed, in words.</param>
    /// <param name="innerException">The transport failure, if any.</param>
    public RealtimeConnectionException(string message, Exception? innerException = null)
        : base(message, innerException)
    {
    }
}

/// <summary>
/// A <see cref="RealtimeClient.Join"/> that would exceed the contract's 200-room limit. The
/// client refuses it before the server has to, so a form that opens too many rooms gets a
/// clear local error instead of a <c>join_error</c> after a round trip.
/// </summary>
public sealed class RealtimeRoomLimitException : InvalidOperationException
{
    /// <summary>Creates the exception for the room that was refused.</summary>
    /// <param name="room">The room name that was refused.</param>
    /// <param name="limit">The limit that was reached.</param>
    public RealtimeRoomLimitException(string room, int limit)
        : base($"Cannot join room '{room}': the client is already in its maximum of {limit} rooms. Leave a room first.")
    {
        Room = room;
        Limit = limit;
    }

    /// <summary>The refused room.</summary>
    public string Room { get; }

    /// <summary>The limit that was reached (200 by contract).</summary>
    public int Limit { get; }
}

/// <summary>Raised when the session is gone and the shell must sign in again.</summary>
public sealed class RealtimeSessionEndedEventArgs : EventArgs
{
    /// <summary>Creates the event arguments.</summary>
    /// <param name="closeCode">The close code that ended the session (4001 or 4009).</param>
    /// <param name="reason">A short, token-free explanation.</param>
    public RealtimeSessionEndedEventArgs(int closeCode, string reason)
    {
        CloseCode = closeCode;
        Reason = reason;
    }

    /// <summary>4001 (authentication failed or timed out) or 4009 (session expired or revoked).</summary>
    public int CloseCode { get; }

    /// <summary>A short explanation that contains no credential.</summary>
    public string Reason { get; }
}

/// <summary>Raised when a subscriber's handler threw. The receive loop is unaffected.</summary>
public sealed class RealtimeHandlerFaultedEventArgs : EventArgs
{
    /// <summary>Creates the event arguments.</summary>
    /// <param name="room">The room whose handler threw.</param>
    /// <param name="exception">The handler's exception.</param>
    public RealtimeHandlerFaultedEventArgs(string room, Exception exception)
    {
        Room = room;
        Exception = exception;
    }

    /// <summary>The room whose handler threw.</summary>
    public string Room { get; }

    /// <summary>The exception the handler threw; its message is the subscriber's own.</summary>
    public Exception Exception { get; }
}
