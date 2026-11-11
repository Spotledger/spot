using System.Threading.Channels;

namespace Spotledger.Client.Realtime;

/// <summary>
/// A room's ordered, bounded handler queue. The receive loop only ever posts into it, so a
/// slow handler can never block the socket; when the queue is full the room reports a slow
/// consumer and the client drops the connection and resyncs instead of growing without bound.
/// </summary>
internal sealed class RealtimeRoomDispatcher : IAsyncDisposable
{
    private readonly Channel<RealtimeRoomMessage> _queue;
    private readonly SynchronizationContext? _context;
    private readonly Func<RealtimeRoomMessage, ValueTask> _deliver;
    private readonly CancellationTokenSource _cts = new();
    private readonly Task _loop;
    private int _disposed;

    /// <summary>Creates a dispatcher for one room.</summary>
    /// <param name="capacity">How many messages may wait for the handlers.</param>
    /// <param name="context">The context handlers run on, or null for the dispatcher's own thread.</param>
    /// <param name="deliver">Invokes the room's handlers for one message.</param>
    public RealtimeRoomDispatcher(int capacity, SynchronizationContext? context, Func<RealtimeRoomMessage, ValueTask> deliver)
    {
        ArgumentNullException.ThrowIfNull(deliver);
        _context = context;
        _deliver = deliver;
        _queue = Channel.CreateBounded<RealtimeRoomMessage>(new BoundedChannelOptions(capacity)
        {
            SingleReader = true,
            SingleWriter = false,
            AllowSynchronousContinuations = false,
            // Wait is what makes TryWrite report "full": the client turns that into a resync
            // rather than an unbounded queue or a silent drop.
            FullMode = BoundedChannelFullMode.Wait,
        });
        _loop = Task.Run(RunAsync);
    }

    /// <summary>
    /// Queues a message for the room's handlers. False means the queue is full: the caller
    /// must treat the connection as a slow consumer.
    /// </summary>
    /// <param name="message">The message for this room.</param>
    public bool TryPost(RealtimeRoomMessage message)
    {
        ArgumentNullException.ThrowIfNull(message);
        return _queue.Writer.TryWrite(message);
    }

    /// <summary>Drops everything still queued, for a reconnect: those events are replayed.</summary>
    /// <returns>How many messages were dropped.</returns>
    public int DropQueued()
    {
        var dropped = 0;
        while (_queue.Reader.TryRead(out _))
        {
            dropped++;
        }

        return dropped;
    }

    /// <inheritdoc/>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }

        _queue.Writer.TryComplete();
        _cts.Cancel();
        try
        {
            await _loop.ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            // Disposal races the loop's own cancellation; nothing to report.
        }

        _cts.Dispose();
    }

    private async Task RunAsync()
    {
        try
        {
            while (await _queue.Reader.WaitToReadAsync(_cts.Token).ConfigureAwait(false))
            {
                while (_queue.Reader.TryRead(out var message))
                {
                    try
                    {
                        await InvokeAsync(message).ConfigureAwait(false);
                    }
                    catch (Exception)
                    {
                        // Deliver guards each handler; a fault here is the dispatcher's own
                        // plumbing and must not end the room's delivery loop.
                    }
                }
            }
        }
        catch (OperationCanceledException)
        {
            // Disposed.
        }
        catch (ChannelClosedException)
        {
            // Disposed.
        }
    }

    private async ValueTask InvokeAsync(RealtimeRoomMessage message)
    {
        if (_context is null)
        {
            await _deliver(message).ConfigureAwait(false);
            return;
        }

        // Handlers run on the captured context (the UI thread in the shell), one at a time
        // and in order, so a subscriber never needs its own marshalling.
        var completion = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        _context.Post(
            static state =>
            {
                var (deliver, queued, signal) = ((Func<RealtimeRoomMessage, ValueTask>, RealtimeRoomMessage, TaskCompletionSource))state!;
                _ = RunOnContextAsync(deliver, queued, signal);
            },
            (_deliver, message, completion));
        await completion.Task.ConfigureAwait(false);
    }

    private static async Task RunOnContextAsync(
        Func<RealtimeRoomMessage, ValueTask> deliver,
        RealtimeRoomMessage message,
        TaskCompletionSource completion)
    {
        try
        {
            await deliver(message).ConfigureAwait(true);
            completion.TrySetResult();
        }
        catch (Exception ex)
        {
            // Deliver already contains each handler's own guard; this is the last resort so a
            // fault can never take the dispatcher loop (and with it the room) down.
            completion.TrySetException(ex);
        }
    }
}
