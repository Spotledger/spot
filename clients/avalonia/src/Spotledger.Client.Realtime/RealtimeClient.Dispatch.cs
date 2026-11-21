namespace Spotledger.Client.Realtime;

/// <summary>Turning parsed server messages into room deliveries, and the seq rules.</summary>
public sealed partial class RealtimeClient
{
    /// <summary>Handles one text frame from the server; a non-null result ends the connection.</summary>
    private Outcome? HandleServerMessage(ReadOnlyMemory<byte> payload)
    {
        IRealtimeServerMessage message;
        try
        {
            message = RealtimeMessageCodec.ParseServerMessage(payload);
        }
        catch (RealtimeProtocolException ex)
        {
            // The server speaks another dialect: a client bug we cannot retry away.
            Log($"protocol fault: {ex.Message}");
            return Outcome.Halt(RealtimeCloseCodes.ProtocolError, ex.Message);
        }

        switch (message)
        {
            case RealtimeJoined joined:
                OnJoined(joined);
                return null;
            case RealtimeJoinError error:
                OnJoinError(error);
                return null;
            case RealtimeEvent @event:
                return OnEvent(@event) ? null : Outcome.Retry("slow consumer: the handler queue is full");
            case RealtimeReady:
                // ready is sent once per connection. A second one is a server quirk, not a
                // reason to reconnect: ignore it (state stays Ready) and count it.
                Interlocked.Increment(ref _duplicateReady);
                Log("a duplicate ready was ignored");
                return null;
            case RealtimePong:
                return null;
            case RealtimeUnknownServerMessage unknown:
                // The server may add message types; ignoring one must not break the stream.
                Interlocked.Increment(ref _unknownServerMessages);
                Log($"ignored a server message of unknown type '{unknown.Type}'");
                return null;
            default:
                Interlocked.Increment(ref _unknownServerMessages);
                return null;
        }
    }

    private void OnJoined(RealtimeJoined joined)
    {
        Room? target = null;
        RealtimeResyncRequired? resync = null;
        lock (_gate)
        {
            _pendingJoins.Remove(joined.Ref);
            if (!_rooms.TryGetValue(joined.Room, out var room))
            {
                // A reply for a room whose last subscriber already left: nothing to deliver.
                return;
            }

            target = room;
            room.OnWire = true;
            switch (joined.Outcome)
            {
                case RealtimeJoinOutcome.ResyncRequired:
                    room.AwaitingResync = true;
                    resync = new RealtimeResyncRequired(joined.Room, RealtimeResyncReason.ResumeExpired, room.LastDeliveredSeq);
                    break;
                case RealtimeJoinOutcome.Replayed:
                    // The replay is contiguous with resume_after, so any resume gap is repaired.
                    room.AwaitingResync = false;
                    break;
                default:
                    room.AwaitingResync = false;
                    break;
            }
        }

        if (resync is not null && !target.Dispatcher.TryPost(resync))
        {
            Interlocked.Increment(ref _droppedMessages);
        }
    }

    private void OnJoinError(RealtimeJoinError error)
    {
        RealtimeRoomDispatcher? dispatcher = null;
        lock (_gate)
        {
            _pendingJoins.Remove(error.Ref);
            if (_rooms.TryGetValue(error.Room, out var room))
            {
                room.OnWire = false;
                // A refused join is not retried on every reconnect: the answer will not change
                // until a new subscriber asks again (Join clears this).
                room.JoinFailed = true;
                dispatcher = room.Dispatcher;
            }
        }

        if (dispatcher is null)
        {
            return;
        }

        Log($"join refused for '{error.Room}': {error.Code}");
        if (!dispatcher.TryPost(error))
        {
            Interlocked.Increment(ref _droppedMessages);
        }
    }

    /// <summary>
    /// The seq rules: a gap means lost messages, so the room is marked for resync and the
    /// event that revealed the gap is not delivered as if nothing happened. Later events are
    /// withheld until the subscriber acknowledges the resync.
    /// </summary>
    /// <returns>False when the handler queue is full (the connection must be dropped).</returns>
    private bool OnEvent(RealtimeEvent @event)
    {
        Room? target = null;
        RealtimeResyncRequired? resync = null;
        var deliver = false;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(@event.Room, out var room))
            {
                // An event for a room we are not in: ignore it (it may arrive just after leave).
                Interlocked.Increment(ref _droppedMessages);
                return true;
            }

            target = room;
            if (room.AwaitingResync)
            {
                // Rebaseline on the stream the server is actually sending; still withheld.
                room.LastReceivedSeq = @event.Seq;
                Interlocked.Increment(ref _droppedMessages);
                return true;
            }

            if (room.LastReceivedSeq == 0)
            {
                // First event on a fresh join has no predecessor to be contiguous with.
                room.LastReceivedSeq = @event.Seq;
                deliver = true;
            }
            else if (@event.Seq <= room.LastReceivedSeq)
            {
                // Already seen (a replay overlapping the resume point, or a duplicate).
                Interlocked.Increment(ref _droppedMessages);
                return true;
            }
            else if (@event.Seq != room.LastReceivedSeq + 1)
            {
                room.LastReceivedSeq = @event.Seq;
                room.AwaitingResync = true;
                resync = new RealtimeResyncRequired(@event.Room, RealtimeResyncReason.Gap, room.LastDeliveredSeq);
                Interlocked.Increment(ref _droppedMessages);
            }
            else
            {
                room.LastReceivedSeq = @event.Seq;
                deliver = true;
            }
        }

        if (resync is not null)
        {
            if (!target.Dispatcher.TryPost(resync))
            {
                Log("slow consumer: the handler queue is full; dropping the connection to resync");
                return false;
            }

            return true;
        }

        if (!deliver)
        {
            return true;
        }

        if (target.Dispatcher.TryPost(@event))
        {
            return true;
        }

        Log("slow consumer: the handler queue is full; dropping the connection to resync");
        return false;
    }

    /// <summary>The dispatcher's delivery callback: update the resume point, then the handlers.</summary>
    private ValueTask DeliverAsync(RealtimeRoomMessage message)
    {
        RealtimeSubscription[] handlers;
        lock (_gate)
        {
            if (!_rooms.TryGetValue(message.Room, out var room))
            {
                return ValueTask.CompletedTask;
            }

            if (message is RealtimeEvent @event && @event.Seq > room.LastDeliveredSeq)
            {
                // The resume point is what the subscriber actually saw, not what arrived: an
                // event still in the queue when the connection drops is replayed, not lost.
                room.LastDeliveredSeq = @event.Seq;
            }

            handlers = room.Handlers.Count == 0 ? [] : [.. room.Handlers];
        }

        foreach (var subscription in handlers)
        {
            try
            {
                subscription.Handler(message);
            }
            catch (Exception ex)
            {
                // A subscriber's bug never kills the receive loop.
                Interlocked.Increment(ref _handlerFaults);
                Log($"a handler for a room threw {ex.GetType().Name}");
                try
                {
                    HandlerFaulted?.Invoke(this, new RealtimeHandlerFaultedEventArgs(message.Room, ex));
                }
                catch (Exception inner)
                {
                    Log($"handler-faulted subscriber failed: {inner.GetType().Name}");
                }
            }
        }

        return ValueTask.CompletedTask;
    }
}
