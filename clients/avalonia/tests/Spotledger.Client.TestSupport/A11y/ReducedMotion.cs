namespace Spotledger.Client.TestSupport.A11y;

/// <summary>
/// The reduced-motion seam (WP-UI-16.T2). The theme catalogue has motion durations
/// (<c>Motion.Fast</c> 120 ms, <c>Motion.Normal</c> 200 ms, <c>Motion.Slow</c> 320 ms) but no
/// reduced-motion token yet, so the bound lives here and is the single place a test changes it.
/// TestSupport only: no production control reads this, and nothing here reads a clock.
/// </summary>
public static class ReducedMotion
{
    /// <summary>
    /// The longest animation allowed to run with reduced motion requested. WCAG 2.3.3 asks for a
    /// way to switch motion off; 100 ms is short enough to read as a state change, not a movement,
    /// and is the value the accessibility review recorded.
    /// </summary>
    public static readonly TimeSpan ReducedBound = TimeSpan.FromMilliseconds(100);

    private static int _scopes;

    /// <summary>True while at least one lease is open.</summary>
    public static bool IsEnabled => Volatile.Read(ref _scopes) > 0;

    /// <summary>The longest allowed duration: the reduced bound when enabled, otherwise no limit.</summary>
    public static TimeSpan Bound => IsEnabled ? ReducedBound : TimeSpan.MaxValue;

    /// <summary>Enable reduced motion until the returned lease is disposed. Nesting is counted.</summary>
    public static IDisposable Enable()
    {
        Interlocked.Increment(ref _scopes);
        return new Lease();
    }

    /// <summary>Clamp a duration to the bound; the shape a reduced-motion policy takes.</summary>
    public static TimeSpan Clamp(TimeSpan duration) => duration <= Bound ? duration : ReducedBound;

    private sealed class Lease : IDisposable
    {
        private int _disposed;

        public void Dispose()
        {
            if (Interlocked.Exchange(ref _disposed, 1) == 0) Interlocked.Decrement(ref _scopes);
        }
    }
}
