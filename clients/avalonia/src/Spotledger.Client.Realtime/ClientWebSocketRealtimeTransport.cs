using System.Net.WebSockets;

namespace Spotledger.Client.Realtime;

/// <summary>
/// <see cref="IRealtimeTransport"/> over <see cref="ClientWebSocket"/>. One instance is one
/// connection: dispose it and ask the factory for a new one to reconnect.
/// </summary>
/// <remarks>
/// The contract has no header-based authentication because browsers cannot set headers on a
/// WebSocket, so this transport sends none: <c>auth</c> is the first message. It also checks
/// that the server actually negotiated <c>spot.v1</c>; an upgrade without it is refused rather
/// than half-working.
/// </remarks>
public sealed class ClientWebSocketRealtimeTransport : IRealtimeTransport
{
    private const int ReceiveBufferSize = 8 * 1024;

    private readonly SemaphoreSlim _sendGate = new(1, 1);
    private ClientWebSocket? _socket;
    private bool _disposed;

    /// <summary>Creates a transport; call <see cref="ConnectAsync"/> to open the socket.</summary>
    public ClientWebSocketRealtimeTransport()
    {
    }

    /// <summary>The subprotocol the server negotiated, or null before a connection.</summary>
    public string? NegotiatedSubProtocol => _socket?.SubProtocol;

    /// <inheritdoc/>
    public async Task ConnectAsync(Uri endpoint, string subProtocol, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(endpoint);
        ArgumentException.ThrowIfNullOrWhiteSpace(subProtocol);
        ObjectDisposedException.ThrowIf(_disposed, this);

        var socket = new ClientWebSocket();
        socket.Options.AddSubProtocol(subProtocol);
        try
        {
            await socket.ConnectAsync(endpoint, cancellationToken).ConfigureAwait(false);
        }
        catch (Exception ex) when (ex is WebSocketException or HttpRequestException or IOException or InvalidOperationException)
        {
            socket.Dispose();
            // The message carries no URL and no credential: only that the upgrade failed.
            throw new RealtimeConnectionException("The realtime WebSocket upgrade failed.", ex);
        }

        if (!string.Equals(socket.SubProtocol, subProtocol, StringComparison.Ordinal))
        {
            var negotiated = socket.SubProtocol;
            socket.Dispose();
            throw new RealtimeConnectionException(
                $"The server did not negotiate the '{subProtocol}' subprotocol (it answered '{negotiated ?? "nothing"}').");
        }

        _socket = socket;
    }

    /// <inheritdoc/>
    public async Task SendTextAsync(ReadOnlyMemory<byte> utf8Json, CancellationToken cancellationToken)
    {
        var socket = _socket ?? throw new RealtimeConnectionException("The realtime transport is not connected.");
        await _sendGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            await socket.SendAsync(utf8Json, WebSocketMessageType.Text, endOfMessage: true, cancellationToken)
                .ConfigureAwait(false);
        }
        finally
        {
            _sendGate.Release();
        }
    }

    /// <inheritdoc/>
    public async Task<RealtimeFrame> ReceiveAsync(CancellationToken cancellationToken)
    {
        var socket = _socket ?? throw new RealtimeConnectionException("The realtime transport is not connected.");
        var buffer = new byte[ReceiveBufferSize];
        var segment = new ArraySegment<byte>(buffer);
        using var accumulator = new MemoryStream(ReceiveBufferSize);
        while (true)
        {
            var result = await socket.ReceiveAsync(segment, cancellationToken).ConfigureAwait(false);
            switch (result.MessageType)
            {
                case WebSocketMessageType.Close:
                    // A socket that dies without a close frame has no status: that is 1006.
                    return RealtimeFrame.Close(result.CloseStatus is { } status ? (int)status : 1006);
                case WebSocketMessageType.Binary:
                    if (accumulator.Length + result.Count > RealtimeProtocol.MaxFrameBytes)
                    {
                        return RealtimeFrame.TooLarge(RealtimeProtocol.MaxFrameBytes + 1);
                    }

                    accumulator.Write(buffer, 0, result.Count);
                    if (result.EndOfMessage)
                    {
                        return RealtimeFrame.Binary(accumulator.ToArray());
                    }

                    break;
                default:
                    if (accumulator.Length + result.Count > RealtimeProtocol.MaxFrameBytes)
                    {
                        // Stop buffering a frame the contract forbids rather than growing
                        // until the heap says no; the client closes 1009 without parsing it.
                        return RealtimeFrame.TooLarge(RealtimeProtocol.MaxFrameBytes + 1);
                    }

                    accumulator.Write(buffer, 0, result.Count);
                    if (result.EndOfMessage)
                    {
                        return RealtimeFrame.Text(accumulator.ToArray());
                    }

                    break;
            }
        }
    }

    /// <inheritdoc/>
    public async Task CloseAsync(int closeCode, string? reason, CancellationToken cancellationToken)
    {
        var socket = _socket;
        if (socket is null)
        {
            return;
        }

        try
        {
            if (socket.State is WebSocketState.Open or WebSocketState.CloseReceived)
            {
                await socket.CloseOutputAsync((WebSocketCloseStatus)closeCode, reason, cancellationToken).ConfigureAwait(false);
            }
        }
        catch (Exception ex) when (ex is WebSocketException or IOException or ObjectDisposedException or OperationCanceledException)
        {
            // Closing a socket that already failed is not an error worth surfacing.
        }
    }

    /// <inheritdoc/>
    public ValueTask DisposeAsync()
    {
        if (_disposed)
        {
            return ValueTask.CompletedTask;
        }

        _disposed = true;
        _socket?.Dispose();
        _socket = null;
        _sendGate.Dispose();
        return ValueTask.CompletedTask;
    }
}
