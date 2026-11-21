namespace Spotledger.Client.Realtime;

/// <summary>The kinds of frame a transport can hand to the client.</summary>
public enum RealtimeFrameKind
{
    /// <summary>One UTF-8 JSON text frame: the only kind the contract allows in either direction.</summary>
    Text = 0,

    /// <summary>A binary frame: the server closes with 1003, so a received one is a protocol fault.</summary>
    Binary = 1,

    /// <summary>A WebSocket ping control frame. It counts as activity and needs no JSON reply.</summary>
    Ping = 2,

    /// <summary>The socket closed; <see cref="RealtimeFrame.CloseCode"/> says why.</summary>
    Close = 3,

    /// <summary>A frame larger than the contract's 64 KiB; the transport stopped buffering it.</summary>
    TooLarge = 4,
}

/// <summary>One frame, as the transport layer sees it.</summary>
public readonly struct RealtimeFrame
{
    private RealtimeFrame(RealtimeFrameKind kind, ReadOnlyMemory<byte> payload, int closeCode, int observedLength)
    {
        Kind = kind;
        Payload = payload;
        CloseCode = closeCode;
        ObservedLength = observedLength;
    }

    /// <summary>What kind of frame this is.</summary>
    public RealtimeFrameKind Kind { get; }

    /// <summary>The UTF-8 payload; empty for close and too-large frames.</summary>
    public ReadOnlyMemory<byte> Payload { get; }

    /// <summary>The close code for <see cref="RealtimeFrameKind.Close"/>; 0 otherwise.</summary>
    public int CloseCode { get; }

    /// <summary>The length the transport saw, for a <see cref="RealtimeFrameKind.TooLarge"/> frame.</summary>
    public int ObservedLength { get; }

    /// <summary>A text frame carrying JSON.</summary>
    /// <param name="payload">The UTF-8 bytes of one JSON object.</param>
    public static RealtimeFrame Text(ReadOnlyMemory<byte> payload) =>
        new(RealtimeFrameKind.Text, payload, 0, payload.Length);

    /// <summary>A binary frame.</summary>
    /// <param name="payload">The bytes the server sent.</param>
    public static RealtimeFrame Binary(ReadOnlyMemory<byte> payload) =>
        new(RealtimeFrameKind.Binary, payload, 0, payload.Length);

    /// <summary>A ping control frame.</summary>
    /// <param name="payload">The (usually empty) ping body.</param>
    public static RealtimeFrame Ping(ReadOnlyMemory<byte> payload) =>
        new(RealtimeFrameKind.Ping, payload, 0, payload.Length);

    /// <summary>A close frame.</summary>
    /// <param name="closeCode">The code from the contract's table, or 1006 when the socket died.</param>
    public static RealtimeFrame Close(int closeCode) =>
        new(RealtimeFrameKind.Close, ReadOnlyMemory<byte>.Empty, closeCode, 0);

    /// <summary>A frame the transport refused to buffer because it exceeds the contract's limit.</summary>
    /// <param name="observedLength">How many bytes were seen before buffering stopped.</param>
    public static RealtimeFrame TooLarge(int observedLength) =>
        new(RealtimeFrameKind.TooLarge, ReadOnlyMemory<byte>.Empty, 0, observedLength);
}

/// <summary>
/// The one seam between <see cref="RealtimeClient"/> and a socket. It connects to
/// <c>wss://site/api/v1/realtime</c> with subprotocol <c>spot.v1</c>, sends one UTF-8 JSON
/// text frame at a time, receives frames, and closes with a code. One instance is one
/// connection: a reconnect asks for a new one from the factory.
/// </summary>
public interface IRealtimeTransport : IAsyncDisposable
{
    /// <summary>Opens the socket and negotiates the subprotocol.</summary>
    /// <param name="endpoint">The upgrade URL.</param>
    /// <param name="subProtocol">The required subprotocol, <c>spot.v1</c>.</param>
    /// <param name="cancellationToken">Cancels the attempt.</param>
    Task ConnectAsync(Uri endpoint, string subProtocol, CancellationToken cancellationToken);

    /// <summary>Sends one text frame.</summary>
    /// <param name="utf8Json">The UTF-8 bytes of one JSON message.</param>
    /// <param name="cancellationToken">Cancels the send.</param>
    Task SendTextAsync(ReadOnlyMemory<byte> utf8Json, CancellationToken cancellationToken);

    /// <summary>Waits for the next frame. Throws when the connection failed.</summary>
    /// <param name="cancellationToken">Cancels the wait.</param>
    Task<RealtimeFrame> ReceiveAsync(CancellationToken cancellationToken);

    /// <summary>Closes the socket with a contract close code.</summary>
    /// <param name="closeCode">The code to send (1000 for a normal close).</param>
    /// <param name="reason">A short reason; never a credential or payload.</param>
    /// <param name="cancellationToken">Cancels the close.</param>
    Task CloseAsync(int closeCode, string? reason, CancellationToken cancellationToken);
}
