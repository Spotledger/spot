
namespace Spotledger.Client.Realtime;

/// <summary>
/// A source of the session token for each connection attempt. The client calls it once per
/// attempt and never stores the result, so the shell can hand out <c>SessionTokenStore</c>'s
/// current value without the realtime layer keeping a credential of its own.
/// </summary>
/// <param name="cancellationToken">Cancelled when the attempt is abandoned.</param>
/// <returns>The token, or null when there is no session.</returns>
public delegate ValueTask<string?> RealtimeTokenSource(CancellationToken cancellationToken);

/// <summary>Everything <see cref="RealtimeClient"/> can be configured with; all of it optional.</summary>
public sealed class RealtimeClientOptions
{
    /// <summary>
    /// The site this client talks to, used for <c>Origin</c>-free diagnostics. The endpoint
    /// itself comes from the client's constructor argument.
    /// </summary>
    public Uri? Site { get; init; }

    /// <summary>
    /// Creates a fresh transport per connection attempt. Defaults to
    /// <see cref="ClientWebSocketRealtimeTransport"/>; tests and the browser head replace it.
    /// </summary>
    public Func<IRealtimeTransport> TransportFactory { get; init; } = static () => new ClientWebSocketRealtimeTransport();

    /// <summary>Time source for backoff, the ready timeout and the heartbeat. Never the wall clock.</summary>
    public TimeProvider TimeProvider { get; init; } = TimeProvider.System;

    /// <summary>Jitter source for the backoff. Tests inject a seeded instance.</summary>
    public Random Jitter { get; init; } = Random.Shared;

    /// <summary>
    /// How many messages one room may queue for its handlers. A handler that falls further
    /// behind than this makes the client drop the connection and resync, which is the
    /// server's own slow-consumer rule (256 messages) applied from this side.
    /// </summary>
    public int HandlerQueueCapacity { get; init; } = 256;

    /// <summary>The room limit enforced before the server refuses a join (200 by contract).</summary>
    public int MaxRooms { get; init; } = RealtimeProtocol.MaxRooms;

    /// <summary>
    /// The context handlers run on, captured once. When null the client captures the
    /// current <see cref="SynchronizationContext"/> (the UI thread in the shell) or runs
    /// handlers on the dispatcher's own thread.
    /// </summary>
    public SynchronizationContext? HandlerContext { get; init; }

    /// <summary>How long to wait for <c>ready</c> after sending <c>auth</c> (the contract's 10 s).</summary>
    public TimeSpan ReadyTimeout { get; init; } = RealtimeProtocol.AuthWindow;

    /// <summary>
    /// Receives one-line, already-redacted diagnostics (protocol faults, dropped frames,
    /// handler failures). Never receives a token, a URL or a payload.
    /// </summary>
    public Action<string>? DiagnosticLog { get; init; }
}

/// <summary>
/// The exponential backoff of <c>realtime.md</c>: the cap starts at 1 s, doubles per attempt
/// and stops at 30 s; the delay is uniform in <c>[0, cap]</c> (full jitter).
/// </summary>
internal sealed class RealtimeBackoff
{
    private readonly Random _random;

    /// <summary>Creates the schedule around the injected jitter source.</summary>
    /// <param name="random">The jitter source; tests pass a seeded instance.</param>
    public RealtimeBackoff(Random random)
    {
        ArgumentNullException.ThrowIfNull(random);
        _random = random;
    }

    /// <summary>The cap for a one-based attempt number: 1 s, 2 s, 4 s ... 30 s.</summary>
    /// <param name="attempt">The attempt number, starting at 1 for the first reconnect.</param>
    public static TimeSpan CapFor(int attempt)
    {
        if (attempt <= 1)
        {
            return RealtimeProtocol.BackoffBase;
        }

        var maxMillis = RealtimeProtocol.BackoffCap.TotalMilliseconds;
        var millis = RealtimeProtocol.BackoffBase.TotalMilliseconds;
        for (var i = 1; i < attempt && millis < maxMillis; i++)
        {
            millis *= 2;
        }

        return TimeSpan.FromMilliseconds(Math.Min(millis, maxMillis));
    }

    /// <summary>The next delay: uniform in <c>[0, cap]</c>, in whole milliseconds.</summary>
    /// <param name="attempt">The attempt number, starting at 1.</param>
    public TimeSpan NextDelay(int attempt)
    {
        var cap = (long)CapFor(attempt).TotalMilliseconds;
        // Full jitter: any value in [0, cap], so a crowd of clients does not reconnect together.
        var millis = _random.NextInt64(0, cap + 1);
        return TimeSpan.FromMilliseconds(millis);
    }
}

/// <summary>Monotonic activity tracking, so the heartbeat never reads the wall clock.</summary>
internal static class RealtimeActivity
{
    /// <summary>The elapsed time since a <see cref="TimeProvider.GetTimestamp"/> reading.</summary>
    public static TimeSpan Since(TimeProvider time, long timestamp)
    {
        ArgumentNullException.ThrowIfNull(time);
        var now = time.GetTimestamp();
        if (now < timestamp)
        {
            // A test clock that was rewound: treat it as no elapsed time rather than a huge one.
            return TimeSpan.Zero;
        }

        return time.GetElapsedTime(timestamp, now);
    }

    /// <summary>Reads the current timestamp from the injected provider.</summary>
    public static long Now(TimeProvider time)
    {
        ArgumentNullException.ThrowIfNull(time);
        return time.GetTimestamp();
    }
}
