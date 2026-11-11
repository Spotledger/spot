namespace Spotledger.Client.Realtime;

/// <summary>
/// One <see cref="RealtimeClient.Join"/>: the subscriber's handle on a room. Disposing it is
/// <see cref="RealtimeClient.Leave"/>; the wire <c>leave</c> is sent only when the room's last
/// subscriber goes away.
/// </summary>
public sealed class RealtimeSubscription : IDisposable
{
    private readonly RealtimeClient _client;
    private int _disposed;

    /// <summary>Creates a subscription; only <see cref="RealtimeClient"/> does this.</summary>
    /// <param name="client">The owning client.</param>
    /// <param name="room">The room name.</param>
    /// <param name="handler">The subscriber's handler.</param>
    internal RealtimeSubscription(RealtimeClient client, string room, Action<RealtimeRoomMessage> handler)
    {
        _client = client;
        Room = room;
        Handler = handler;
    }

    /// <summary>The room this subscription listens to.</summary>
    public string Room { get; }

    /// <summary>The subscriber's handler, invoked in order on the captured context.</summary>
    internal Action<RealtimeRoomMessage> Handler { get; }

    /// <summary>True once <see cref="Dispose"/> ran.</summary>
    public bool IsDisposed => Volatile.Read(ref _disposed) != 0;

    /// <summary>
    /// Tells the client the subscriber has refetched the room's data after a
    /// <see cref="RealtimeResyncRequired"/>. Events for the room are withheld until this is
    /// called, so nothing is shown as current while it is known to be stale.
    /// </summary>
    public void AcknowledgeResync()
    {
        ObjectDisposedException.ThrowIf(IsDisposed, this);
        _client.AcknowledgeResync(Room);
    }

    /// <summary>Leaves the room for this subscriber only.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }

        _client.Unsubscribe(this);
    }
}
