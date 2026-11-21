using System.Text.Json;
using Spotledger.Client.Realtime;

namespace Spotledger.Client.Notifications.Tests;

/// <summary>A fake <see cref="IRoomSource"/>: records joins and lets a test deliver messages.</summary>
internal sealed class FakeRooms : IRoomSource
{
    private readonly object _gate = new();
    private readonly List<FakeSubscription> _joined = [];

    public int Active
    {
        get
        {
            lock (_gate)
            {
                return _joined.Count(s => !s.Disposed);
            }
        }
    }

    public IReadOnlyList<string> Rooms
    {
        get
        {
            lock (_gate)
            {
                return [.. _joined.Where(s => !s.Disposed).Select(s => s.Room)];
            }
        }
    }

    public int Acknowledged { get; set; }

    public IRoomSubscription Join(string room, Action<RealtimeRoomMessage> handler)
    {
        var subscription = new FakeSubscription(this, room, handler);
        lock (_gate)
        {
            _joined.Add(subscription);
        }

        return subscription;
    }

    public void Deliver(string room, RealtimeRoomMessage message)
    {
        FakeSubscription[] targets;
        lock (_gate)
        {
            targets = [.. _joined.Where(s => !s.Disposed && s.Room == room)];
        }

        foreach (var target in targets)
        {
            target.Handler(message);
        }
    }

    private sealed class FakeSubscription(FakeRooms owner, string room, Action<RealtimeRoomMessage> handler) : IRoomSubscription
    {
        public string Room { get; } = room;

        public Action<RealtimeRoomMessage> Handler { get; } = handler;

        public bool Disposed { get; private set; }

        public void AcknowledgeResync() => owner.Acknowledged++;

        public void Dispose() => Disposed = true;
    }
}

internal static class Events
{
    public static RealtimeEvent Notice(string eventId, string? title = "Invoice paid", string? body = null, string? severity = null,
        string? doctype = null, string? id = null, long seq = 1)
    {
        var data = new Dictionary<string, object?>();
        if (title is not null) data["title"] = title;
        if (body is not null) data["body"] = body;
        if (severity is not null) data["severity"] = severity;
        if (doctype is not null) data["doctype"] = doctype;
        if (id is not null) data["id"] = id;
        var payload = JsonSerializer.SerializeToElement(new Dictionary<string, object?> { ["event_id"] = eventId, ["event"] = "user_notice", ["data"] = data });
        return new RealtimeEvent("user:u1", seq, RealtimeEventKind.Notification, "notification", payload, new DateTimeOffset(2026, 10, 30, 9, 0, 0, TimeSpan.Zero));
    }

    public static RealtimeEvent Doc(RealtimeEventKind kind, string doctype, string id, int version, long seq = 1)
    {
        var payload = JsonSerializer.SerializeToElement(new Dictionary<string, object?> { ["doctype"] = doctype, ["id"] = id, ["version"] = version });
        return new RealtimeEvent($"doc:{doctype}:{id}", seq, kind, kind.ToString(), payload, new DateTimeOffset(2026, 10, 30, 9, 0, 0, TimeSpan.Zero));
    }
}

internal sealed class FakeTime : TimeProvider
{
    private readonly object _gate = new();
    private readonly List<FakeTimer> _timers = [];
    private DateTimeOffset _now = new(2026, 10, 30, 9, 0, 0, TimeSpan.Zero);

    public override DateTimeOffset GetUtcNow()
    {
        lock (_gate)
        {
            return _now;
        }
    }

    public override ITimer CreateTimer(TimerCallback callback, object? state, TimeSpan dueTime, TimeSpan period)
    {
        var timer = new FakeTimer(this, callback, state, _now + dueTime);
        lock (_gate)
        {
            _timers.Add(timer);
        }

        return timer;
    }

    public void Advance(TimeSpan delta)
    {
        List<FakeTimer> due;
        lock (_gate)
        {
            _now += delta;
            due = [.. _timers.Where(t => !t.Disposed && t.DueAt <= _now)];
            foreach (var timer in due)
            {
                _timers.Remove(timer);
            }
        }

        foreach (var timer in due.OrderBy(t => t.DueAt))
        {
            timer.Callback(timer.State);
        }
    }

    private sealed class FakeTimer(FakeTime owner, TimerCallback callback, object? state, DateTimeOffset dueAt) : ITimer
    {
        public TimerCallback Callback { get; } = callback;

        public object? State { get; } = state;

        public DateTimeOffset DueAt { get; } = dueAt;

        public bool Disposed { get; private set; }

        public bool Change(TimeSpan dueTime, TimeSpan period) => false;

        public void Dispose()
        {
            Disposed = true;
            lock (owner._gate)
            {
                owner._timers.Remove(this);
            }
        }

        public ValueTask DisposeAsync()
        {
            Dispose();
            return ValueTask.CompletedTask;
        }
    }
}

internal sealed class FakeDocument(string doctype, string id, int version, bool dirty = false) : IWatchedDocument
{
    public string DocType { get; } = doctype;

    public string Id { get; } = id;

    public int LoadedVersion { get; set; } = version;

    public bool IsDirty { get; set; } = dirty;

    public int Reloads { get; private set; }

    public Task ReloadAsync()
    {
        Reloads++;
        IsDirty = false;
        return Task.CompletedTask;
    }
}
