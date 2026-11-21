using System.Net.WebSockets;
using System.Threading.Channels;

namespace Spotledger.Client.Realtime;

/// <summary>The connection loop: open, handshake, receive, reconnect.</summary>
public sealed partial class RealtimeClient
{
    /// <summary>What the loop does next after a connection ends.</summary>
    private enum OutcomeKind
    {
        /// <summary>Back off and connect again, resuming every room.</summary>
        Retry = 0,

        /// <summary>Stop for good (normal close, or a protocol fault of the peer).</summary>
        Halt = 1,

        /// <summary>Stop and tell the shell to sign in again (4001/4009).</summary>
        SignOut = 2,
    }

    /// <summary>One decision about a finished connection.</summary>
    private readonly record struct Outcome(OutcomeKind Kind, int CloseCode, string Reason)
    {
        public static Outcome Retry(string reason) => new(OutcomeKind.Retry, 0, reason);

        public static Outcome Halt(int closeCode, string reason) => new(OutcomeKind.Halt, closeCode, reason);

        public static Outcome SignOut(int closeCode, string reason) => new(OutcomeKind.SignOut, closeCode, reason);
    }

    /// <summary>The server closed the socket before it sent <c>ready</c>.</summary>
    private sealed class HandshakeClosedException : Exception
    {
        public HandshakeClosedException(int closeCode)
            : base($"The server closed the connection during the handshake ({closeCode}).")
        {
            CloseCode = closeCode;
        }

        public int CloseCode { get; }
    }

    /// <summary>One connection: its transport, cancellation, outbound queue and heartbeat.</summary>
    private sealed class Session : IDisposable
    {
        public Session(IRealtimeTransport transport, CancellationTokenSource cts)
        {
            Transport = transport;
            Cts = cts;
        }

        public IRealtimeTransport Transport { get; }

        public CancellationTokenSource Cts { get; }

        public Channel<RealtimeClientMessage> Outbound { get; } =
            Channel.CreateUnbounded<RealtimeClientMessage>(new UnboundedChannelOptions
            {
                SingleReader = true,
                SingleWriter = false,
                AllowSynchronousContinuations = false,
            });

        public Task? SendLoop { get; set; }

        public ITimer? Heartbeat { get; set; }

        public TimeSpan HeartbeatPeriod { get; set; } = TimeSpan.FromSeconds(RealtimeProtocol.DefaultHeartbeatSeconds);

        public long LastActivity { get; set; }

        /// <summary>Set when the client itself decided to drop this connection.</summary>
        public Outcome? Abort { get; set; }

        public void Dispose()
        {
            Heartbeat?.Dispose();
            Cts.Dispose();
        }
    }

    /// <summary>The reconnect loop; one instance per <see cref="ConnectAsync(string, CancellationToken)"/>.</summary>
    private async Task RunAsync(RealtimeTokenSource tokenSource, CancellationToken cancellationToken)
    {
        var attempt = 0;
        var everReady = false;
        while (!cancellationToken.IsCancellationRequested)
        {
            if (attempt > 0)
            {
                SetState(RealtimeConnectionState.Reconnecting);
            }

            Session? session = null;
            Outcome outcome;
            try
            {
                session = await OpenSessionAsync(tokenSource, attempt == 0, cancellationToken).ConfigureAwait(false);
                everReady = true;
                attempt = 0;
                lock (_gate)
                {
                    _session = session;
                }

                SetState(RealtimeConnectionState.Ready);
                _readySignal?.TrySetResult(true);
                outcome = await PumpAsync(session, cancellationToken).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                outcome = Outcome.Halt(0, "cancelled");
            }
            catch (HandshakeClosedException ex)
            {
                outcome = ClassifyClose(ex.CloseCode);
            }
            catch (RealtimeConnectionException ex)
            {
                Log($"connection attempt failed: {ex.Message}");
                outcome = Outcome.Retry("connection attempt failed");
            }
            catch (RealtimeProtocolException ex)
            {
                Log($"protocol fault: {ex.Message}");
                outcome = Outcome.Halt(RealtimeCloseCodes.ProtocolError, ex.Message);
            }
            catch (Exception ex) when (ex is IOException or WebSocketException or ObjectDisposedException or InvalidOperationException or OperationCanceledException)
            {
                Log($"transport fault: {ex.GetType().Name}");
                outcome = Outcome.Retry("transport fault");
            }
            finally
            {
                if (session is not null)
                {
                    lock (_gate)
                    {
                        if (ReferenceEquals(_session, session))
                        {
                            _session = null;
                        }
                    }

                    await CloseSessionAsync(session).ConfigureAwait(false);
                }
            }

            if (outcome.Kind != OutcomeKind.Retry)
            {
                Finish(outcome, everReady);
                return;
            }

            attempt++;
            SetState(RealtimeConnectionState.Reconnecting);
            var delay = _backoff.NextDelay(attempt);
            Log($"reconnecting in {(long)delay.TotalMilliseconds} ms (attempt {attempt})");
            try
            {
                await Task.Delay(delay, _time, cancellationToken).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                FinishCancelled();
                return;
            }
        }

        FinishCancelled();
    }

    private async Task<Session> OpenSessionAsync(
        RealtimeTokenSource tokenSource,
        bool firstAttempt,
        CancellationToken cancellationToken)
    {
        if (firstAttempt)
        {
            SetState(RealtimeConnectionState.Connecting);
        }

        var transport = _options.TransportFactory()
            ?? throw new RealtimeConnectionException("The transport factory returned no transport.");
        var session = new Session(transport, CancellationTokenSource.CreateLinkedTokenSource(cancellationToken));
        try
        {
            await transport.ConnectAsync(_endpoint, RealtimeProtocol.SubProtocol, session.Cts.Token).ConfigureAwait(false);
            var token = await tokenSource(session.Cts.Token).ConfigureAwait(false);
            if (string.IsNullOrEmpty(token))
            {
                throw new RealtimeConnectionException("No session token is available for the realtime handshake.");
            }

            // auth is the first message of every connection; nothing else is sent until ready.
            await transport.SendTextAsync(RealtimeMessageCodec.Serialize(new RealtimeAuthMessage(token)), session.Cts.Token)
                .ConfigureAwait(false);
            session.LastActivity = RealtimeActivity.Now(_time);
            var ready = await WaitForReadyAsync(session).ConfigureAwait(false);
            session.HeartbeatPeriod = TimeSpan.FromSeconds(
                ready.HeartbeatSeconds > 0 ? ready.HeartbeatSeconds : RealtimeProtocol.DefaultHeartbeatSeconds);
            UserId = ready.User;
            return session;
        }
        catch
        {
            await transport.DisposeAsync().ConfigureAwait(false);
            session.Dispose();
            throw;
        }
    }

    private async Task<RealtimeReady> WaitForReadyAsync(Session session)
    {
        using var deadline = new CancellationTokenSource(_options.ReadyTimeout, _time);
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(session.Cts.Token, deadline.Token);
        try
        {
            while (true)
            {
                var frame = await session.Transport.ReceiveAsync(linked.Token).ConfigureAwait(false);
                switch (frame.Kind)
                {
                    case RealtimeFrameKind.Text:
                        session.LastActivity = RealtimeActivity.Now(_time);
                        var message = RealtimeMessageCodec.ParseServerMessage(frame.Payload);
                        if (message is RealtimeReady ready)
                        {
                            return ready;
                        }

                        throw new RealtimeProtocolException("The server sent a message before ready.");
                    case RealtimeFrameKind.Ping:
                        session.LastActivity = RealtimeActivity.Now(_time);
                        break;
                    case RealtimeFrameKind.Binary:
                        throw new RealtimeProtocolException("The server sent a binary frame.");
                    case RealtimeFrameKind.TooLarge:
                        throw new RealtimeProtocolException("The server sent a frame larger than 64 KiB.");
                    case RealtimeFrameKind.Close:
                        throw new HandshakeClosedException(frame.CloseCode);
                }
            }
        }
        catch (OperationCanceledException) when (deadline.IsCancellationRequested && !session.Cts.IsCancellationRequested)
        {
            throw new RealtimeConnectionException("The server did not send ready in time.");
        }
    }

    /// <summary>Receives until the connection ends; returns what to do next.</summary>
    private async Task<Outcome> PumpAsync(Session session, CancellationToken cancellationToken)
    {
        StartHeartbeat(session);
        BeginReady(session);
        try
        {
            while (true)
            {
                var frame = await session.Transport.ReceiveAsync(session.Cts.Token).ConfigureAwait(false);
                Interlocked.Increment(ref _receivedFrames);
                if (session.Abort is { } aborted)
                {
                    return aborted;
                }

                switch (frame.Kind)
                {
                    case RealtimeFrameKind.Text:
                        session.LastActivity = RealtimeActivity.Now(_time);
                        if (frame.Payload.Length > RealtimeProtocol.MaxFrameBytes)
                        {
                            // Refused before parsing: the contract says a frame over 64 KiB is 1009.
                            return Outcome.Halt(RealtimeCloseCodes.MessageTooBig, "a frame exceeded 64 KiB");
                        }

                        if (HandleServerMessage(frame.Payload) is { } step)
                        {
                            return step;
                        }

                        break;
                    case RealtimeFrameKind.Ping:
                        // The transport answers the control frame; the application only notes
                        // that the connection is alive.
                        session.LastActivity = RealtimeActivity.Now(_time);
                        break;
                    case RealtimeFrameKind.Binary:
                        return Outcome.Halt(RealtimeCloseCodes.UnsupportedData, "the server sent a binary frame");
                    case RealtimeFrameKind.TooLarge:
                        return Outcome.Halt(RealtimeCloseCodes.MessageTooBig, "a frame exceeded 64 KiB");
                    case RealtimeFrameKind.Close:
                        return ClassifyClose(frame.CloseCode);
                }
            }
        }
        catch (OperationCanceledException) when (session.Abort is not null)
        {
            return session.Abort.Value;
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            return Outcome.Halt(0, "cancelled");
        }
        catch (Exception ex) when (ex is IOException or WebSocketException or ObjectDisposedException or InvalidOperationException or OperationCanceledException)
        {
            Log($"transport fault: {ex.GetType().Name}");
            return Outcome.Retry("transport fault");
        }
    }

    /// <summary>
    /// The first thing after <c>ready</c>: reset each room's seq bookkeeping to what the
    /// subscribers actually saw, drop what was queued but not handled (the server replays it
    /// from <c>resume_after</c>), then rejoin every room.
    /// </summary>
    private void BeginReady(Session session)
    {
        List<Room> joins = [];
        lock (_gate)
        {
            foreach (var room in _rooms.Values)
            {
                room.OnWire = false;
                room.LastReceivedSeq = room.LastDeliveredSeq;
                Interlocked.Add(ref _droppedMessages, room.Dispatcher.DropQueued());
                if (room.Handlers.Count > 0 && !room.JoinFailed)
                {
                    room.OnWire = true;
                    joins.Add(room);
                }
            }

            _pendingJoins.Clear();
        }

        session.SendLoop = Task.Run(() => SendLoopAsync(session), CancellationToken.None);
        foreach (var room in joins)
        {
            SendJoin(room);
        }
    }

    private async Task SendLoopAsync(Session session)
    {
        var reader = session.Outbound.Reader;
        try
        {
            while (await reader.WaitToReadAsync(session.Cts.Token).ConfigureAwait(false))
            {
                while (reader.TryRead(out var message))
                {
                    try
                    {
                        await session.Transport
                            .SendTextAsync(RealtimeMessageCodec.Serialize(message), session.Cts.Token)
                            .ConfigureAwait(false);
                    }
                    catch (Exception ex) when (ex is IOException or WebSocketException or ObjectDisposedException or InvalidOperationException or OperationCanceledException)
                    {
                        Log($"send failed: {ex.GetType().Name}");
                        RequestAbort(session, Outcome.Retry("send failed"));
                        return;
                    }
                }
            }
        }
        catch (OperationCanceledException)
        {
            // The connection ended.
        }
        catch (ChannelClosedException)
        {
            // The connection ended.
        }
    }

    private void StartHeartbeat(Session session)
    {
        var period = session.HeartbeatPeriod;
        var tick = period < TimeSpan.FromMilliseconds(10) ? TimeSpan.FromMilliseconds(10) : period;
        session.Heartbeat = _time.CreateTimer(_ => CheckHeartbeat(session), null, tick, tick);
    }

    private void CheckHeartbeat(Session session)
    {
        if (session.Abort is not null)
        {
            return;
        }

        var limit = session.HeartbeatPeriod * RealtimeProtocol.HeartbeatMissMultiplier;
        if (RealtimeActivity.Since(_time, session.LastActivity) >= limit)
        {
            // Nothing arrived for two heartbeats: the socket may look open and be dead. Treat
            // it as a fault and reconnect, resuming every room.
            Log("heartbeat timeout; reconnecting");
            RequestAbort(session, Outcome.Retry("heartbeat timeout"));
        }
    }

    private void RequestAbort(Session session, Outcome outcome)
    {
        lock (_gate)
        {
            session.Abort ??= outcome;
        }

        try
        {
            session.Cts.Cancel();
        }
        catch (ObjectDisposedException)
        {
            // The connection already ended.
        }
    }

    private static Outcome ClassifyClose(int closeCode) => closeCode switch
    {
        RealtimeCloseCodes.Normal => Outcome.Halt(closeCode, "the server closed the connection normally"),
        RealtimeCloseCodes.GoingAway => Outcome.Retry("the server is going away"),
        RealtimeCloseCodes.UnsupportedData => Outcome.Halt(closeCode, "a binary frame was exchanged (client bug)"),
        RealtimeCloseCodes.ProtocolError => Outcome.Halt(closeCode, "protocol violation (client bug)"),
        RealtimeCloseCodes.MessageTooBig => Outcome.Halt(closeCode, "a frame was too large (client bug)"),
        RealtimeCloseCodes.AuthenticationFailed => Outcome.SignOut(closeCode, "authentication failed or timed out"),
        // 4008 is the server's slow-consumer close: reconnect and resume, and expect the
        // server to answer the join with resync_required.
        RealtimeCloseCodes.SlowConsumer => Outcome.Retry("slow consumer"),
        RealtimeCloseCodes.SessionExpired => Outcome.SignOut(closeCode, "the session expired or was revoked"),
        _ => Outcome.Retry($"the connection closed with {closeCode}"),
    };

    private void Finish(Outcome outcome, bool everReady)
    {
        switch (outcome.Kind)
        {
            case OutcomeKind.SignOut:
                SetState(RealtimeConnectionState.SignedOut);
                Log($"session ended ({outcome.CloseCode}): {outcome.Reason}");
                RaiseSessionEnded(outcome.CloseCode, outcome.Reason);
                _readySignal?.TrySetException(new RealtimeConnectionException(
                    $"The realtime session ended ({outcome.CloseCode}): {outcome.Reason}"));
                break;
            case OutcomeKind.Halt when outcome.CloseCode is RealtimeCloseCodes.UnsupportedData
                or RealtimeCloseCodes.ProtocolError
                or RealtimeCloseCodes.MessageTooBig:
                // A bug in this client: one redacted diagnostic, then stop. Retrying would loop.
                Log($"fatal protocol fault ({outcome.CloseCode}): {outcome.Reason}");
                SetState(RealtimeConnectionState.Closed);
                _readySignal?.TrySetException(new RealtimeConnectionException(outcome.Reason));
                break;
            default:
                SetState(RealtimeConnectionState.Closed);
                if (!everReady)
                {
                    _readySignal?.TrySetException(
                        new RealtimeConnectionException("The realtime connection ended before ready."));
                }

                break;
        }
    }

    private void FinishCancelled()
    {
        SetState(RealtimeConnectionState.Closed);
        _readySignal?.TrySetCanceled();
    }

    private static async Task CloseSessionAsync(Session session)
    {
        session.Heartbeat?.Dispose();
        session.Heartbeat = null;
        session.Outbound.Writer.TryComplete();
        try
        {
            await session.Cts.CancelAsync().ConfigureAwait(false);
        }
        catch (ObjectDisposedException)
        {
            // Already gone.
        }

        if (session.SendLoop is { } loop)
        {
            try
            {
                await loop.ConfigureAwait(false);
            }
            catch (Exception ex) when (ex is OperationCanceledException or ChannelClosedException)
            {
                // The connection ended; the send loop had nothing left to do.
            }
        }

        try
        {
            // A normal close code: the connection is being dropped on purpose, and a faulted
            // socket ignores this anyway.
            await session.Transport.CloseAsync(RealtimeCloseCodes.Normal, null, CancellationToken.None)
                .ConfigureAwait(false);
        }
        catch (Exception ex) when (ex is IOException or WebSocketException or ObjectDisposedException)
        {
            // Best effort.
        }

        await session.Transport.DisposeAsync().ConfigureAwait(false);
        session.Dispose();
    }

    private void RaiseSessionEnded(int closeCode, string reason)
    {
        try
        {
            SessionEnded?.Invoke(this, new RealtimeSessionEndedEventArgs(closeCode, reason));
        }
        catch (Exception ex)
        {
            Log($"session-ended subscriber failed: {ex.GetType().Name}");
        }
    }
}
