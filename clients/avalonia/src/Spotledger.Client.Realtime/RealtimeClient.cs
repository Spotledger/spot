namespace Spotledger.Client.Realtime;

/// <summary>
/// The client half of <c>contracts/native-api/realtime.md</c> v0.1: one connection per
/// session, one wire <c>join</c> per room however many subscribers, per-room <c>seq</c>
/// tracking with gap detection, and reconnect with resume.
/// </summary>
/// <remarks>
/// <para>
/// Realtime is notification, not data sync (ADR-25). No correctness may depend on a message
/// arriving: a subscriber treats every <see cref="RealtimeEvent"/> as a hint, refetches what
/// it shows after <see cref="RealtimeResyncRequired"/>, and is correct if nothing arrives at
/// all.
/// </para>
/// <para>
/// Handlers run in order per room on the context captured at construction (the UI thread in
/// the shell) from a bounded queue. A handler that falls behind the stream makes the client
/// drop the connection and resync, exactly as the server's slow-consumer rule does.
/// </para>
/// </remarks>
public sealed partial class RealtimeClient : IAsyncDisposable
{
    private readonly Uri _site;
    private readonly Uri _endpoint;
    private readonly RealtimeClientOptions _options;
    private readonly TimeProvider _time;
    private readonly RealtimeBackoff _backoff;
    private readonly SynchronizationContext? _handlerContext;
    private readonly object _gate = new();
    private readonly Dictionary<string, Room> _rooms = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> _pendingJoins = new(StringComparer.Ordinal);

    private RealtimeConnectionState _state = RealtimeConnectionState.Closed;
    private Session? _session;
    private CancellationTokenSource? _runCts;
    private Task? _runTask;
    private TaskCompletionSource<bool>? _readySignal;
    private long _nextRef;
    private long _receivedFrames;
    private long _unknownServerMessages;
    private long _duplicateReady;
    private long _droppedMessages;
    private long _handlerFaults;
    private int _disposed;

    /// <summary>Creates the client for one site. Nothing is opened until <see cref="ConnectAsync(string, CancellationToken)"/>.</summary>
    /// <param name="site">The site's HTTP(S) base address; the endpoint is derived from it.</param>
    /// <param name="options">Options; all defaulted.</param>
    public RealtimeClient(Uri site, RealtimeClientOptions? options = null)
    {
        ArgumentNullException.ThrowIfNull(site);
        _site = site;
        _endpoint = RealtimeProtocol.EndpointFor(site);
        _options = options ?? new RealtimeClientOptions();
        _time = _options.TimeProvider;
        _backoff = new RealtimeBackoff(_options.Jitter);
        _handlerContext = _options.HandlerContext ?? SynchronizationContext.Current;
        if (_options.HandlerQueueCapacity < 1)
        {
            throw new ArgumentOutOfRangeException(nameof(options), "HandlerQueueCapacity must be at least 1.");
        }

        if (_options.MaxRooms is < 1 or > RealtimeProtocol.MaxRooms)
        {
            throw new ArgumentOutOfRangeException(
                nameof(options),
                $"MaxRooms must be between 1 and the contract's {RealtimeProtocol.MaxRooms}.");
        }
    }

    /// <summary>The endpoint this client connects to.</summary>
    public Uri Endpoint => _endpoint;

    /// <summary>The site the endpoint belongs to.</summary>
    public Uri Site => _site;

    /// <summary>The lifecycle state; see the README's table.</summary>
    public RealtimeConnectionState State
    {
        get
        {
            lock (_gate)
            {
                return _state;
            }
        }
    }

    /// <summary>The signed-in user from <c>ready</c>, or null before it.</summary>
    public string? UserId { get; private set; }

    /// <summary>How many distinct rooms the client is in (or waiting to join).</summary>
    public int RoomCount
    {
        get
        {
            lock (_gate)
            {
                return _rooms.Count;
            }
        }
    }

    /// <summary>Server messages of a type this version does not know: ignored, counted.</summary>
    public long UnknownServerMessageCount => Interlocked.Read(ref _unknownServerMessages);

    /// <summary>Frames received on the current (and earlier) connections, including pings.</summary>
    public long ReceivedFrameCount => Interlocked.Read(ref _receivedFrames);

    /// <summary>A second <c>ready</c> on one connection: ignored (idempotent), counted.</summary>
    public long DuplicateReadyCount => Interlocked.Read(ref _duplicateReady);

    /// <summary>Events withheld because the room awaited a resync, or queued when the connection dropped.</summary>
    public long DroppedMessageCount => Interlocked.Read(ref _droppedMessages);

    /// <summary>Handler exceptions caught, so the receive loop survived them.</summary>
    public long HandlerFaultCount => Interlocked.Read(ref _handlerFaults);

    /// <summary>Raised when <see cref="State"/> changes (on the connection's thread: marshal it).</summary>
    public event EventHandler<RealtimeConnectionState>? StateChanged;

    /// <summary>Raised on 4001/4009: the shell runs its re-sign-in flow and reconnects.</summary>
    public event EventHandler<RealtimeSessionEndedEventArgs>? SessionEnded;

    /// <summary>Raised when a subscriber's handler threw. The connection is unaffected.</summary>
    public event EventHandler<RealtimeHandlerFaultedEventArgs>? HandlerFaulted;

    /// <summary>
    /// Connects, sends <c>auth</c> and waits for <c>ready</c>. The token is held in memory for
    /// reconnects only: it is never logged, persisted, or written into an exception.
    /// </summary>
    /// <param name="token">The session token from the login response.</param>
    /// <param name="cancellationToken">Cancels the wait for <c>ready</c>.</param>
    public Task ConnectAsync(string token, CancellationToken cancellationToken)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(token);
        return ConnectCoreAsync(_ => ValueTask.FromResult<string?>(token), cancellationToken);
    }

    /// <summary>
    /// Connects with a token source asked once per attempt, so the shell can resolve the
    /// current <c>SessionTokenStore</c> value without this layer keeping a credential.
    /// </summary>
    /// <param name="tokenSource">Called once per connection attempt.</param>
    /// <param name="cancellationToken">Cancels the wait for <c>ready</c>.</param>
    public Task ConnectAsync(RealtimeTokenSource tokenSource, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(tokenSource);
        return ConnectCoreAsync(tokenSource, cancellationToken);
    }

    /// <summary>
    /// Subscribes to a room. The wire <c>join</c> is sent once per room however many
    /// subscribers there are, and the contract's 200-room limit is enforced here, before the
    /// server has to refuse.
    /// </summary>
    /// <param name="room">The room name, exactly as the contract spells it (<c>doc:doctype:id</c>, <c>doctype:doctype</c>, <c>user:id</c>, <c>site</c>).</param>
    /// <param name="handler">Receives the room's events, join errors and resync requests, in order.</param>
    /// <returns>A subscription; disposing it leaves the room for this subscriber.</returns>
    /// <exception cref="RealtimeRoomLimitException">The client is already in its maximum number of rooms.</exception>
    public RealtimeSubscription Join(string room, Action<RealtimeRoomMessage> handler)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(room);
        ArgumentNullException.ThrowIfNull(handler);

        RealtimeSubscription subscription;
        Room? joinNow = null;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(room, out var state))
            {
                if (_rooms.Count >= _options.MaxRooms)
                {
                    throw new RealtimeRoomLimitException(room, _options.MaxRooms);
                }

                state = CreateRoom(room);
                _rooms.Add(room, state);
            }
            else if (state.JoinFailed)
            {
                // A new subscriber deserves its own attempt: the refusal may have been for
                // the old one's permission, and the server re-checks anyway.
                state.JoinFailed = false;
            }

            subscription = new RealtimeSubscription(this, room, handler);
            state.Handlers.Add(subscription);
            if (_state == RealtimeConnectionState.Ready && !state.OnWire && !state.JoinFailed)
            {
                state.OnWire = true;
                joinNow = state;
            }
        }

        if (joinNow is not null)
        {
            SendJoin(joinNow);
        }

        return subscription;
    }

    /// <summary>Leaves one subscriber's room; the wire <c>leave</c> follows the last one.</summary>
    /// <param name="room">The room name.</param>
    public void Leave(string room)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(room);
        RealtimeSubscription? last;
        lock (_gate)
        {
            last = _rooms.TryGetValue(room, out var state) && state.Handlers.Count > 0
                ? state.Handlers[^1]
                : null;
        }

        last?.Dispose();
    }

    /// <summary>Answers a server ping with the contract's <c>ping</c> message.</summary>
    public bool SendPing() => TrySend(new RealtimePingMessage());

    /// <summary>Closes the connection normally (1000) and stops reconnecting.</summary>
    public async Task DisconnectAsync()
    {
        Task? run;
        CancellationTokenSource? cts;
        lock (_gate)
        {
            run = _runTask;
            cts = _runCts;
        }

        if (run is null)
        {
            SetState(RealtimeConnectionState.Closed);
            return;
        }

        if (run.IsCompleted)
        {
            // The loop already ended and decided its own state (SignedOut, Closed, fatal):
            // a later disconnect must not overwrite that decision.
            return;
        }

        try
        {
            cts?.Cancel();
            await run.ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            // Cancellation is how a normal disconnect ends the loop.
        }
    }

    /// <inheritdoc/>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }

        await DisconnectAsync().ConfigureAwait(false);
        Room[] rooms;
        lock (_gate)
        {
            rooms = [.. _rooms.Values];
            _rooms.Clear();
            _pendingJoins.Clear();
        }

        foreach (var room in rooms)
        {
            await room.Dispatcher.DisposeAsync().ConfigureAwait(false);
        }

        lock (_gate)
        {
            _runCts?.Dispose();
            _runCts = null;
        }
    }

    /// <summary>Removes a subscription; called by <see cref="RealtimeSubscription.Dispose"/>.</summary>
    /// <param name="subscription">The subscription going away.</param>
    internal void Unsubscribe(RealtimeSubscription subscription)
    {
        ArgumentNullException.ThrowIfNull(subscription);
        var sendLeave = false;
        RealtimeRoomDispatcher? dispatcher = null;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(subscription.Room, out var room))
            {
                return;
            }

            if (!room.Handlers.Remove(subscription))
            {
                return;
            }

            if (room.Handlers.Count == 0)
            {
                _rooms.Remove(room.Name);
                dispatcher = room.Dispatcher;
                // Leaving a room the server never joined us to is a no-op there; still correct.
                sendLeave = room.OnWire;
            }
        }

        if (sendLeave)
        {
            TrySend(new RealtimeLeaveMessage(subscription.Room));
        }

        if (dispatcher is not null)
        {
            _ = dispatcher.DisposeAsync().AsTask();
        }
    }

    /// <summary>Clears the room's resync state; called by <see cref="RealtimeSubscription.AcknowledgeResync"/>.</summary>
    /// <param name="room">The room that was refetched.</param>
    internal void AcknowledgeResync(string room)
    {
        lock (_gate)
        {
            if (_rooms.TryGetValue(room, out var state))
            {
                state.AwaitingResync = false;
            }
        }
    }

    private async Task ConnectCoreAsync(RealtimeTokenSource tokenSource, CancellationToken cancellationToken)
    {
        ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
        Task ready;
        CancellationTokenSource cts;
        lock (_gate)
        {
            if (_runTask is { IsCompleted: false })
            {
                throw new InvalidOperationException("The realtime client is already connected.");
            }

            _readySignal = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
            _runCts?.Dispose();
            _runCts = new CancellationTokenSource();
            cts = _runCts;
            _state = RealtimeConnectionState.Connecting;
            _runTask = Task.Run(() => RunAsync(tokenSource, cts.Token), CancellationToken.None);
            ready = _readySignal.Task;
        }

        RaiseStateChanged(RealtimeConnectionState.Connecting);

        try
        {
            await ready.WaitAsync(cancellationToken).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            await DisconnectAsync().ConfigureAwait(false);
            throw;
        }
    }

    private Room CreateRoom(string name)
    {
        var dispatcher = new RealtimeRoomDispatcher(
            _options.HandlerQueueCapacity,
            _handlerContext,
            DeliverAsync);
        var room = new Room(name, dispatcher);
        return room;
    }

    private void SendJoin(Room room)
    {
        var roomRef = "j" + Interlocked.Increment(ref _nextRef).ToString(System.Globalization.CultureInfo.InvariantCulture);
        long? resumeAfter;
        lock (_gate)
        {
            _pendingJoins[roomRef] = room.Name;
            resumeAfter = room.LastDeliveredSeq > 0 ? room.LastDeliveredSeq : null;
        }

        if (!TrySend(new RealtimeJoinMessage(roomRef, room.Name, resumeAfter)))
        {
            lock (_gate)
            {
                _pendingJoins.Remove(roomRef);
                room.OnWire = false;
            }
        }
    }

    private bool TrySend(RealtimeClientMessage message)
    {
        Session? session;
        lock (_gate)
        {
            session = _session;
        }

        return session is not null && session.Outbound.Writer.TryWrite(message);
    }

    private void SetState(RealtimeConnectionState state)
    {
        var changed = false;
        lock (_gate)
        {
            if (_state != state)
            {
                _state = state;
                changed = true;
            }
        }

        if (changed)
        {
            RaiseStateChanged(state);
        }
    }

    private void RaiseStateChanged(RealtimeConnectionState state)
    {
        try
        {
            StateChanged?.Invoke(this, state);
        }
        catch (Exception ex)
        {
            // A shell subscriber that throws must not take the connection down.
            Log($"state-changed subscriber failed: {ex.GetType().Name}");
        }
    }

    private void Log(string message)
    {
        try
        {
            _options.DiagnosticLog?.Invoke(message);
        }
        catch (Exception)
        {
            // Diagnostics never break the client.
        }
    }

    /// <summary>One room's state: subscribers, wire state and the seq bookkeeping.</summary>
    private sealed class Room
    {
        public Room(string name, RealtimeRoomDispatcher dispatcher)
        {
            Name = name;
            Dispatcher = dispatcher;
        }

        public string Name { get; }

        public RealtimeRoomDispatcher Dispatcher { get; }

        public List<RealtimeSubscription> Handlers { get; } = [];

        /// <summary>A <c>join</c> is outstanding or acknowledged on the current connection.</summary>
        public bool OnWire { get; set; }

        /// <summary>The server refused the join; do not retry it on every reconnect.</summary>
        public bool JoinFailed { get; set; }

        /// <summary>The stream is untrustworthy until the subscriber acknowledges a refetch.</summary>
        public bool AwaitingResync { get; set; }

        /// <summary>The highest <c>seq</c> received on the current connection.</summary>
        public long LastReceivedSeq { get; set; }

        /// <summary>The highest <c>seq</c> handed to the subscribers: the resume point.</summary>
        public long LastDeliveredSeq { get; set; }
    }
}
