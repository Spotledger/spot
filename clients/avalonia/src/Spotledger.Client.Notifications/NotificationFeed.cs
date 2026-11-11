using Spotledger.Client.Realtime;

namespace Spotledger.Client.Notifications;

/// <summary>
/// The user's notices while connected. Subscribes to <c>user:&lt;id&gt;</c>, keeps the last
/// <see cref="RetainedNotices"/> (newest first), counts the unread and remembers which ids were read
/// (ids and a flag only, pruned to the retained window). Contract v0.8.0 has no notice list endpoint, so
/// nothing is invented on first connect or after a resync: the feed shows what arrives while connected.
/// Delivery is at-least-once, so a repeated <c>event_id</c> is ignored.
/// </summary>
public sealed class NotificationFeed : IDisposable
{
    /// <summary>How many notices are kept.</summary>
    public const int RetainedNotices = 200;

    private readonly object _gate = new();
    private readonly IRoomSource _rooms;
    private readonly INoticeStateStore _store;
    private readonly string _server;
    private readonly string _user;
    private readonly Func<bool> _appIsFocused;
    private readonly List<Notice> _notices = [];
    private readonly List<string> _readIds;
    private readonly HashSet<string> _seen = new(StringComparer.Ordinal);
    private readonly Queue<string> _seenOrder = new();
    private IRoomSubscription? _subscription;
    private bool _doNotDisturb;
    private bool _autoReloadClean;
    private bool _disposed;

    /// <summary>Creates the feed over the realtime rooms.</summary>
    /// <param name="rooms">Where to join.</param>
    /// <param name="store">Where read state is kept.</param>
    /// <param name="server">The server the person is signed in to (part of the state's key).</param>
    /// <param name="user">The user id (the room <c>user:&lt;id&gt;</c>).</param>
    /// <param name="appIsFocused">Whether the app has focus; a toast is raised only then.</param>
    public NotificationFeed(IRoomSource rooms, INoticeStateStore store, string server, string user, Func<bool>? appIsFocused = null)
    {
        _rooms = rooms ?? throw new ArgumentNullException(nameof(rooms));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        ArgumentException.ThrowIfNullOrWhiteSpace(server);
        ArgumentException.ThrowIfNullOrWhiteSpace(user);
        _server = server;
        _user = user;
        _appIsFocused = appIsFocused ?? (() => true);
        var state = store.Load(server, user);
        _readIds = [.. state.ReadIds.TakeLast(NoticeStateStore.MaxReadIds)];
        _doNotDisturb = state.DoNotDisturb;
        _autoReloadClean = state.AutoReloadClean;
    }

    /// <summary>Raised after the list or the unread count changed (any thread).</summary>
    public event EventHandler? Changed;

    /// <summary>Raised for a new notice that deserves a toast: the app is focused and do-not-disturb is off.</summary>
    public event EventHandler<Notice>? ToastRequested;

    /// <summary>The notices, newest first.</summary>
    public IReadOnlyList<Notice> Notices
    {
        get
        {
            lock (_gate)
            {
                return [.. _notices];
            }
        }
    }

    /// <summary>How many retained notices are unread.</summary>
    public int UnreadCount
    {
        get
        {
            lock (_gate)
            {
                return _notices.Count(n => !IsReadLocked(n.EventId));
            }
        }
    }

    /// <summary>Whether a notice was read.</summary>
    public bool IsRead(string eventId)
    {
        lock (_gate)
        {
            return IsReadLocked(eventId);
        }
    }

    /// <summary>How many times the server asked for a resync; there is nothing to refetch, so it only counts.</summary>
    public int ResyncCount { get; private set; }

    /// <summary>The "do not disturb" setting: toasts off, the bell still counts. Saved locally.</summary>
    public bool DoNotDisturb
    {
        get => _doNotDisturb;
        set => SetFlag(ref _doNotDisturb, value);
    }

    /// <summary>The "reload clean forms by themselves" setting (default off). Saved locally.</summary>
    public bool AutoReloadClean
    {
        get => _autoReloadClean;
        set => SetFlag(ref _autoReloadClean, value);
    }

    /// <summary>Joins the user's room. Calling it twice has no effect.</summary>
    public void Start()
    {
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            if (_subscription is not null)
            {
                return;
            }
        }

        var subscription = _rooms.Join("user:" + _user, OnMessage);
        lock (_gate)
        {
            _subscription = subscription;
        }
    }

    /// <summary>Marks one notice read.</summary>
    public void MarkRead(string eventId)
    {
        lock (_gate)
        {
            if (!_notices.Exists(n => n.EventId == eventId) || IsReadLocked(eventId))
            {
                return;
            }

            AddReadLocked(eventId);
            SaveLocked();
        }

        Changed?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Marks every retained notice read.</summary>
    public void MarkAllRead()
    {
        lock (_gate)
        {
            foreach (var notice in _notices.Where(n => !IsReadLocked(n.EventId)).ToList())
            {
                AddReadLocked(notice.EventId);
            }

            SaveLocked();
        }

        Changed?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Leaves the room.</summary>
    public void Dispose()
    {
        IRoomSubscription? subscription;
        lock (_gate)
        {
            _disposed = true;
            subscription = _subscription;
            _subscription = null;
        }

        subscription?.Dispose();
    }

    private void OnMessage(RealtimeRoomMessage message)
    {
        switch (message)
        {
            case RealtimeEvent realtimeEvent when NoticeParser.TryParse(realtimeEvent, out var notice):
                Add(notice);
                break;
            case RealtimeResyncRequired:
                // No endpoint lists missed notices (contract v0.8.0): acknowledge so events flow again.
                ResyncCount++;
                IRoomSubscription? subscription;
                lock (_gate)
                {
                    subscription = _subscription;
                }

                subscription?.AcknowledgeResync();
                break;
        }
    }

    private void Add(Notice notice)
    {
        lock (_gate)
        {
            if (_disposed || !_seen.Add(notice.EventId))
            {
                return;
            }

            _seenOrder.Enqueue(notice.EventId);
            while (_seenOrder.Count > RetainedNotices * 4)
            {
                _seen.Remove(_seenOrder.Dequeue());
            }

            _notices.Insert(0, notice);
            if (_notices.Count > RetainedNotices)
            {
                _notices.RemoveRange(RetainedNotices, _notices.Count - RetainedNotices);
            }
        }

        Changed?.Invoke(this, EventArgs.Empty);
        if (!IsRead(notice.EventId) && !DoNotDisturb && _appIsFocused())
        {
            ToastRequested?.Invoke(this, notice);
        }
    }

    private bool IsReadLocked(string eventId) => _readIds.Contains(eventId);

    private void AddReadLocked(string eventId)
    {
        _readIds.Add(eventId);
        if (_readIds.Count > NoticeStateStore.MaxReadIds)
        {
            _readIds.RemoveRange(0, _readIds.Count - NoticeStateStore.MaxReadIds);
        }
    }

    private void SaveLocked() => _store.Save(_server, _user, new NoticeState([.. _readIds], _doNotDisturb, _autoReloadClean));

    private void SetFlag(ref bool field, bool value)
    {
        lock (_gate)
        {
            if (field == value)
            {
                return;
            }

            field = value;
            SaveLocked();
        }

        Changed?.Invoke(this, EventArgs.Empty);
    }
}
