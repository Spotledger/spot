using System.Diagnostics;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>
/// A <see cref="TimeProvider"/> whose clock only moves when a test says so, with working
/// timers. <c>Task.Delay(delay, provider)</c>, <c>CancellationTokenSource(delay, provider)</c>
/// and the client's heartbeat all run on it, so a 30 s backoff is a function call.
/// </summary>
internal sealed class ManualTimeProvider : TimeProvider
{
    private readonly object _gate = new();
    private readonly List<VirtualTimer> _timers = [];
    private long _nowTicks;

    public ManualTimeProvider(DateTimeOffset? start = null)
    {
        _nowTicks = (start ?? new DateTimeOffset(2026, 10, 29, 9, 0, 0, TimeSpan.Zero)).UtcTicks;
    }

    public override long TimestampFrequency => TimeSpan.TicksPerSecond;

    public override TimeZoneInfo LocalTimeZone => TimeZoneInfo.Utc;

    public override DateTimeOffset GetUtcNow()
    {
        lock (_gate)
        {
            return new DateTimeOffset(_nowTicks, TimeSpan.Zero);
        }
    }

    public override long GetTimestamp()
    {
        lock (_gate)
        {
            return _nowTicks;
        }
    }

    public override ITimer CreateTimer(TimerCallback callback, object? state, TimeSpan dueTime, TimeSpan period)
    {
        ArgumentNullException.ThrowIfNull(callback);
        var timer = new VirtualTimer(this, callback, state);
        timer.Change(dueTime, period);
        return timer;
    }

    /// <summary>Timers created and not yet fired (one-shot) or disposed.</summary>
    public int PendingTimerCount
    {
        get
        {
            lock (_gate)
            {
                return _timers.Count;
            }
        }
    }

    /// <summary>The earliest due time among pending timers, or null when none is pending.</summary>
    public DateTimeOffset? NextDue
    {
        get
        {
            lock (_gate)
            {
                long? earliest = null;
                foreach (var timer in _timers)
                {
                    if (timer.DueAtLocked is { } due && (earliest is null || due < earliest))
                    {
                        earliest = due;
                    }
                }

                return earliest is { } ticks ? new DateTimeOffset(ticks, TimeSpan.Zero) : null;
            }
        }
    }

    /// <summary>Moves the clock forward and fires every timer that becomes due, in order.</summary>
    /// <param name="delta">How far to move.</param>
    public void Advance(TimeSpan delta)
    {
        ArgumentOutOfRangeException.ThrowIfLessThan(delta, TimeSpan.Zero);
        lock (_gate)
        {
            _nowTicks += delta.Ticks;
        }

        while (true)
        {
            VirtualTimer? next = null;
            var due = long.MaxValue;
            lock (_gate)
            {
                foreach (var timer in _timers)
                {
                    if (timer.DueAt is { } dueAt && dueAt <= _nowTicks && dueAt < due)
                    {
                        next = timer;
                        due = dueAt;
                    }
                }
            }

            if (next is null)
            {
                return;
            }

            next.Fire(_nowTicks);
        }
    }

    /// <summary>Moves the clock forward without firing timers.</summary>
    /// <param name="delta">How far to move.</param>
    public void Skip(TimeSpan delta)
    {
        lock (_gate)
        {
            _nowTicks += delta.Ticks;
        }
    }

    private void Unregister(VirtualTimer timer)
    {
        lock (_gate)
        {
            _timers.Remove(timer);
        }
    }

    private void Register(VirtualTimer timer)
    {
        lock (_gate)
        {
            if (!_timers.Contains(timer))
            {
                _timers.Add(timer);
            }
        }
    }

    private sealed class VirtualTimer : ITimer
    {
        private readonly ManualTimeProvider _owner;
        private readonly TimerCallback _callback;
        private readonly object? _state;
        private long? _dueAt;
        private long _periodTicks;

        public VirtualTimer(ManualTimeProvider owner, TimerCallback callback, object? state)
        {
            _owner = owner;
            _callback = callback;
            _state = state;
        }

        public long? DueAtLocked => _dueAt;

        public long? DueAt
        {
            get
            {
                lock (_owner._gate)
                {
                    return _dueAt;
                }
            }
        }

        public bool Change(TimeSpan dueTime, TimeSpan period)
        {
            lock (_owner._gate)
            {
                _periodTicks = period == Timeout.InfiniteTimeSpan ? 0 : Math.Max(0, period.Ticks);
                if (dueTime == Timeout.InfiniteTimeSpan)
                {
                    _dueAt = null;
                }
                else
                {
                    _dueAt = _owner._nowTicks + Math.Max(0, dueTime.Ticks);
                }

                if (_dueAt is null)
                {
                    _owner._timers.Remove(this);
                }
            }

            if (_dueAt is not null)
            {
                _owner.Register(this);
            }

            return true;
        }

        public void Fire(long now)
        {
            lock (_owner._gate)
            {
                if (_periodTicks > 0)
                {
                    _dueAt = now + _periodTicks;
                }
                else
                {
                    _dueAt = null;
                    _owner._timers.Remove(this);
                }
            }

            _callback(_state);
        }

        public void Dispose()
        {
            lock (_owner._gate)
            {
                _dueAt = null;
                _owner._timers.Remove(this);
            }
        }

        public ValueTask DisposeAsync()
        {
            Dispose();
            return ValueTask.CompletedTask;
        }
    }
}
