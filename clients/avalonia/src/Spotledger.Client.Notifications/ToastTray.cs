namespace Spotledger.Client.Notifications;

/// <summary>
/// At most one toast visible; the rest wait their turn. A visible toast dismisses itself after
/// <see cref="Duration"/> of the injected clock, unless the pointer is over it or focus is in it
/// (<see cref="Pause"/> and <see cref="Resume"/>, counted), in which case the time left is kept.
/// </summary>
public sealed class ToastTray(TimeProvider time, TimeSpan? duration = null) : IDisposable
{
    /// <summary>The default time a toast stays: no toast token exists yet, so this is the one place it is set.</summary>
    public static readonly TimeSpan DefaultDuration = TimeSpan.FromSeconds(6);

    /// <summary>The most toasts waiting; older waiting toasts are dropped first.</summary>
    public const int MaxWaiting = 20;

    private readonly object _gate = new();
    private readonly Queue<Notice> _waiting = new();
    private ITimer? _timer;
    private int _pauses;
    private TimeSpan _remaining;
    private DateTimeOffset _startedAt;
    private bool _disposed;

    /// <summary>How long one toast stays visible.</summary>
    public TimeSpan Duration { get; } = duration ?? DefaultDuration;

    /// <summary>The toast on screen, or null.</summary>
    public Notice? Current { get; private set; }

    /// <summary>How many toasts wait behind the visible one.</summary>
    public int Waiting
    {
        get
        {
            lock (_gate)
            {
                return _waiting.Count;
            }
        }
    }

    /// <summary>Raised when <see cref="Current"/> changed (any thread).</summary>
    public event EventHandler? CurrentChanged;

    /// <summary>Shows the notice now, or queues it behind the visible one.</summary>
    public void Enqueue(Notice notice)
    {
        ArgumentNullException.ThrowIfNull(notice);
        var changed = false;
        lock (_gate)
        {
            if (_disposed)
            {
                return;
            }

            if (Current is null)
            {
                Show(notice);
                changed = true;
            }
            else
            {
                _waiting.Enqueue(notice);
                while (_waiting.Count > MaxWaiting)
                {
                    _waiting.Dequeue();
                }
            }
        }

        if (changed)
        {
            CurrentChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>Holds the countdown (the pointer entered, or focus arrived). Calls nest.</summary>
    public void Pause()
    {
        lock (_gate)
        {
            if (_pauses++ == 0 && Current is not null)
            {
                _remaining = Max(TimeSpan.Zero, _remaining - (time.GetUtcNow() - _startedAt));
                _timer?.Dispose();
                _timer = null;
            }
        }
    }

    /// <summary>Releases one hold; the countdown restarts with the time left when none remain.</summary>
    public void Resume()
    {
        lock (_gate)
        {
            if (_pauses == 0 || --_pauses > 0 || Current is null)
            {
                return;
            }

            StartTimer(_remaining);
        }
    }

    /// <summary>Dismisses the visible toast now and shows the next one.</summary>
    public void Dismiss()
    {
        lock (_gate)
        {
            if (Current is null)
            {
                return;
            }

            Next();
        }

        CurrentChanged?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Stops the countdown and drops everything waiting.</summary>
    public void Dispose()
    {
        lock (_gate)
        {
            _disposed = true;
            _timer?.Dispose();
            _timer = null;
            _waiting.Clear();
            Current = null;
        }
    }

    private void Show(Notice notice)
    {
        Current = notice;
        _remaining = Duration;
        if (_pauses == 0)
        {
            StartTimer(Duration);
        }
    }

    private void StartTimer(TimeSpan due)
    {
        _timer?.Dispose();
        _startedAt = time.GetUtcNow();
        _remaining = due;
        var current = Current;
        _timer = time.CreateTimer(_ => Expire(current), null, due, Timeout.InfiniteTimeSpan);
    }

    private void Expire(Notice? expected)
    {
        lock (_gate)
        {
            if (_disposed || !ReferenceEquals(Current, expected) || _pauses > 0)
            {
                return;
            }

            Next();
        }

        CurrentChanged?.Invoke(this, EventArgs.Empty);
    }

    private void Next()
    {
        _timer?.Dispose();
        _timer = null;
        Current = null;
        if (_waiting.Count > 0)
        {
            Show(_waiting.Dequeue());
        }
    }

    private static TimeSpan Max(TimeSpan a, TimeSpan b) => a > b ? a : b;
}
