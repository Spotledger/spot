namespace Spotledger.Client.Realtime.Tests;

/// <summary>
/// Clock helpers that wait for the client's timer to exist before moving time, so a slow
/// machine cannot make a test advance the manual clock before the timer is created and lose it.
/// </summary>
internal static class ClockExtensions
{
    /// <summary>Waits (without advancing) until a timer is pending, then advances by <paramref name="delta"/>.</summary>
    /// <param name="clock">The manual clock.</param>
    /// <param name="delta">How far to move.</param>
    /// <param name="because">What timer the test expects, for the failure message.</param>
    /// <param name="timeoutMilliseconds">The bound on the real-time wait.</param>
    public static async Task AdvanceWhenTimerAsync(
        this ManualTimeProvider clock,
        TimeSpan delta,
        string because,
        int timeoutMilliseconds = 5000)
    {
        ArgumentNullException.ThrowIfNull(clock);
        await WaitForTimerAsync(clock, () => clock.PendingTimerCount > 0, because, timeoutMilliseconds).ConfigureAwait(false);
        clock.Advance(delta);
    }

    /// <summary>
    /// Like <see cref="AdvanceWhenTimerAsync"/>, but waits until some pending timer is due within
    /// <paramref name="delta"/>, so a lingering unrelated timer (the heartbeat) cannot satisfy the wait.
    /// </summary>
    /// <param name="clock">The manual clock.</param>
    /// <param name="delta">How far to move.</param>
    /// <param name="because">What timer the test expects, for the failure message.</param>
    /// <param name="timeoutMilliseconds">The bound on the real-time wait.</param>
    public static async Task AdvanceWhenTimerDueAsync(
        this ManualTimeProvider clock,
        TimeSpan delta,
        string because,
        int timeoutMilliseconds = 5000)
    {
        ArgumentNullException.ThrowIfNull(clock);
        await WaitForTimerAsync(
            clock,
            () => clock.NextDue is { } due && due <= clock.GetUtcNow() + delta,
            because,
            timeoutMilliseconds).ConfigureAwait(false);
        clock.Advance(delta);
    }

    /// <summary>
    /// Advances by <paramref name="step"/> each time a timer is pending until <paramref name="condition"/>
    /// holds, for places where the number of timers is not known.
    /// </summary>
    /// <param name="clock">The manual clock.</param>
    /// <param name="condition">What must become true.</param>
    /// <param name="step">How far to move each time.</param>
    /// <param name="because">What the test waits for, for the failure message.</param>
    /// <param name="timeoutMilliseconds">The bound on the real-time wait.</param>
    public static async Task AdvanceUntilAsync(
        this ManualTimeProvider clock,
        Func<bool> condition,
        TimeSpan step,
        string because,
        int timeoutMilliseconds = 5000)
    {
        ArgumentNullException.ThrowIfNull(clock);
        ArgumentNullException.ThrowIfNull(condition);
        var watch = System.Diagnostics.Stopwatch.StartNew();
        while (!condition())
        {
            if (watch.ElapsedMilliseconds > timeoutMilliseconds)
            {
                throw new TimeoutException(Describe(clock, because));
            }

            if (clock.PendingTimerCount > 0)
            {
                clock.Advance(step);
            }

            await Task.Delay(2).ConfigureAwait(false);
        }
    }

    private static async Task WaitForTimerAsync(ManualTimeProvider clock, Func<bool> ready, string because, int timeoutMilliseconds)
    {
        var watch = System.Diagnostics.Stopwatch.StartNew();
        while (!ready())
        {
            if (watch.ElapsedMilliseconds > timeoutMilliseconds)
            {
                throw new TimeoutException(Describe(clock, because));
            }

            await Task.Delay(2).ConfigureAwait(false);
        }
    }

    private static string Describe(ManualTimeProvider clock, string because) =>
        $"Timed out waiting for a timer for {because}: PendingTimerCount={clock.PendingTimerCount}, NextDue={clock.NextDue?.ToString("O") ?? "none"}.";
}
