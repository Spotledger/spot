using System.Threading.Channels;

namespace Spotledger.Client.Realtime;

/// <summary>
/// An in-memory <see cref="IRealtimeTransport"/> pair: two endpoints joined by channels, so a
/// test (or the shell's own integration harness) can drive the client without a socket. It is
/// a test double that ships in the library because the shell's view models need one too.
/// </summary>
public sealed class InMemoryRealtimeTransport : IRealtimeTransport, IDisposable
{
    private readonly Channel<RealtimeFrame> _inbound;
    private InMemoryRealtimeTransport? _peer;
    private bool _connected;
    private bool _aborted;
    private bool _disposed;

    private InMemoryRealtimeTransport()
    {
        _inbound = Channel.CreateUnbounded<RealtimeFrame>(new UnboundedChannelOptions
        {
            SingleReader = true,
            SingleWriter = false,
            AllowSynchronousContinuations = false,
        });
    }

    /// <summary>The endpoint for the machine under test.</summary>
    public static InMemoryRealtimeTransportPair CreatePair()
    {
        var client = new InMemoryRealtimeTransport();
        var server = new InMemoryRealtimeTransport();
        client._peer = server;
        server._peer = client;
        return new InMemoryRealtimeTransportPair(client, server);
    }

    /// <summary>The endpoint the connection was opened against, once connected.</summary>
    public Uri? Endpoint { get; private set; }

    /// <summary>The subprotocol the client asked for, once connected.</summary>
    public string? NegotiatedSubProtocol { get; private set; }

    /// <summary>True once both ends are joined.</summary>
    public bool IsConnected => _connected && !_disposed;

    /// <inheritdoc/>
    public Task ConnectAsync(Uri endpoint, string subProtocol, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(endpoint);
        ArgumentException.ThrowIfNullOrWhiteSpace(subProtocol);
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (!string.Equals(subProtocol, RealtimeProtocol.SubProtocol, StringComparison.Ordinal))
        {
            throw new RealtimeConnectionException($"The in-memory peer only speaks '{RealtimeProtocol.SubProtocol}'.");
        }

        cancellationToken.ThrowIfCancellationRequested();
        _connected = true;
        Endpoint = endpoint;
        NegotiatedSubProtocol = subProtocol;
        if (_peer is not null)
        {
            _peer._connected = true;
            _peer.Endpoint = endpoint;
            _peer.NegotiatedSubProtocol = subProtocol;
        }

        return Task.CompletedTask;
    }

    /// <inheritdoc/>
    public Task SendTextAsync(ReadOnlyMemory<byte> utf8Json, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return SendFrameAsync(RealtimeFrame.Text(utf8Json.ToArray()));
    }

    /// <summary>Sends a WebSocket ping control frame, which the contract's server sends every heartbeat.</summary>
    /// <param name="cancellationToken">Cancels the send.</param>
    public Task SendPingAsync(CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return SendFrameAsync(RealtimeFrame.Ping(ReadOnlyMemory<byte>.Empty));
    }

    /// <summary>Sends a binary frame; only a misbehaving peer does this, so tests use it.</summary>
    /// <param name="payload">The bytes to send.</param>
    /// <param name="cancellationToken">Cancels the send.</param>
    public Task SendBinaryAsync(ReadOnlyMemory<byte> payload, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return SendFrameAsync(RealtimeFrame.Binary(payload.ToArray()));
    }

    /// <summary>
    /// Kills this endpoint's socket with no close frame, the way a dropped network does: the
    /// other end's next receive throws.
    /// </summary>
    public void Abort()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _inbound.Writer.TryComplete();
        if (_peer is not null)
        {
            _peer._aborted = true;
            _peer._inbound.Writer.TryComplete();
            _peer._disposed = true;
        }
    }

    /// <inheritdoc/>
    public async Task<RealtimeFrame> ReceiveAsync(CancellationToken cancellationToken)
    {
        while (true)
        {
            if (_inbound.Reader.TryRead(out var frame))
            {
                return frame;
            }

            bool more;
            try
            {
                more = await _inbound.Reader.WaitToReadAsync(cancellationToken).ConfigureAwait(false);
            }
            catch (ChannelClosedException)
            {
                more = false;
            }

            if (!more)
            {
                if (_aborted)
                {
                    throw new IOException("The realtime connection was reset.");
                }

                throw new IOException("The realtime connection ended without a close frame.");
            }
        }
    }

    /// <inheritdoc/>
    public Task CloseAsync(int closeCode, string? reason, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        if (_disposed)
        {
            return Task.CompletedTask;
        }

        _disposed = true;
        if (_peer is not null && !_peer._disposed)
        {
            _peer._inbound.Writer.TryWrite(RealtimeFrame.Close(closeCode));
            _peer._inbound.Writer.TryComplete();
        }

        _inbound.Writer.TryComplete();
        return Task.CompletedTask;
    }

    /// <inheritdoc/>
    public ValueTask DisposeAsync()
    {
        Dispose();
        return ValueTask.CompletedTask;
    }

    /// <summary>Releases the endpoint's channels.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _inbound.Writer.TryComplete();
    }

    private Task SendFrameAsync(RealtimeFrame frame)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        var peer = _peer ?? throw new IOException("The realtime connection has no peer.");
        if (!_connected || peer._disposed || peer._aborted)
        {
            throw new IOException("The realtime connection is closed.");
        }

        if (!peer._inbound.Writer.TryWrite(frame))
        {
            throw new IOException("The realtime connection is closed.");
        }

        return Task.CompletedTask;
    }
}

/// <summary>The two ends of an <see cref="InMemoryRealtimeTransport"/> connection.</summary>
/// <param name="Client">The end <see cref="RealtimeClient"/> owns.</param>
/// <param name="Server">The end a scripted server drives.</param>
public sealed record InMemoryRealtimeTransportPair(InMemoryRealtimeTransport Client, InMemoryRealtimeTransport Server);
