using Spotledger.Client.Realtime;

namespace Spotledger.Client.Notifications;

/// <summary>One subscription to a room.</summary>
public interface IRoomSubscription : IDisposable
{
    /// <summary>Says the subscriber refetched what the room covers after a resync request.</summary>
    void AcknowledgeResync();
}

/// <summary>Where rooms are joined: the realtime client in the app, a fake in tests.</summary>
public interface IRoomSource
{
    /// <summary>Joins a room; disposing the result leaves it for this subscriber.</summary>
    IRoomSubscription Join(string room, Action<RealtimeRoomMessage> handler);
}

/// <summary>The realtime client as a room source. Its join is reference counted: one wire join per room.</summary>
public sealed class RealtimeRoomSource(RealtimeClient client) : IRoomSource
{
    /// <inheritdoc/>
    public IRoomSubscription Join(string room, Action<RealtimeRoomMessage> handler) =>
        new Wrapper(client.Join(room, handler));

    private sealed class Wrapper(RealtimeSubscription inner) : IRoomSubscription
    {
        public void AcknowledgeResync()
        {
            if (!inner.IsDisposed)
            {
                inner.AcknowledgeResync();
            }
        }

        public void Dispose() => inner.Dispose();
    }
}
