using Xunit;

namespace Spotledger.Client.Realtime.Tests;

public sealed class ManualTimeProviderTests
{
    [Fact]
    public async Task A_timer_created_late_in_real_time_is_still_fired_by_the_helper()
    {
        var clock = new ManualTimeProvider();
        var fired = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);

        var consumer = Task.Run(async () =>
        {
            await Task.Delay(200).ConfigureAwait(false);
            await Task.Delay(TimeSpan.FromSeconds(1), clock).ConfigureAwait(false);
            fired.SetResult();
        });

        // The advance is requested before the timer exists: the helper must wait, not lose it.
        await clock.AdvanceWhenTimerAsync(TimeSpan.FromSeconds(1), "the late delay");
        await fired.Task.WithTimeoutAsync("the late timer to fire");
        await consumer;
    }

    [Fact]
    public async Task The_due_variant_ignores_a_pending_timer_that_cannot_fire()
    {
        var clock = new ManualTimeProvider();
        using var far = clock.CreateTimer(_ => { }, null, TimeSpan.FromMinutes(10), Timeout.InfiniteTimeSpan);
        var fired = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var consumer = Task.Run(async () =>
        {
            await Task.Delay(200).ConfigureAwait(false);
            await Task.Delay(TimeSpan.FromSeconds(1), clock).ConfigureAwait(false);
            fired.SetResult();
        });

        await clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the near delay");
        await fired.Task.WithTimeoutAsync("the near timer to fire");
        await consumer;
    }

    [Fact]
    public async Task Advance_with_no_timer_pending_is_not_remembered()
    {
        var clock = new ManualTimeProvider();
        clock.Advance(TimeSpan.FromSeconds(1));

        var delay = Task.Delay(TimeSpan.FromSeconds(1), clock);
        await Task.Delay(50);

        Assert.False(delay.IsCompleted);
        Assert.Equal(1, clock.PendingTimerCount);
        clock.Advance(TimeSpan.FromSeconds(1));
        await delay.WithTimeoutAsync("the delay once time moves with the timer pending");
    }

    [Fact]
    public void Pending_count_and_next_due_follow_the_timers()
    {
        var clock = new ManualTimeProvider();
        Assert.Equal(0, clock.PendingTimerCount);
        Assert.Null(clock.NextDue);

        var start = clock.GetUtcNow();
        using var later = clock.CreateTimer(_ => { }, null, TimeSpan.FromSeconds(30), Timeout.InfiniteTimeSpan);
        using var sooner = clock.CreateTimer(_ => { }, null, TimeSpan.FromSeconds(5), Timeout.InfiniteTimeSpan);
        Assert.Equal(2, clock.PendingTimerCount);
        Assert.Equal(start + TimeSpan.FromSeconds(5), clock.NextDue);

        clock.Advance(TimeSpan.FromSeconds(5));
        Assert.Equal(1, clock.PendingTimerCount);
        Assert.Equal(start + TimeSpan.FromSeconds(30), clock.NextDue);

        later.Dispose();
        Assert.Equal(0, clock.PendingTimerCount);
    }

    [Fact]
    public async Task The_timeout_names_the_reason_and_the_pending_count()
    {
        var clock = new ManualTimeProvider();
        using var far = clock.CreateTimer(_ => { }, null, TimeSpan.FromMinutes(10), Timeout.InfiniteTimeSpan);

        var error = await Assert.ThrowsAsync<TimeoutException>(
            () => clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the missing delay", timeoutMilliseconds: 100));

        Assert.Contains("the missing delay", error.Message, StringComparison.Ordinal);
        Assert.Contains("PendingTimerCount=1", error.Message, StringComparison.Ordinal);
        Assert.Contains("NextDue=", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Advance_until_steps_while_timers_are_pending_until_the_condition_holds()
    {
        var clock = new ManualTimeProvider();
        var count = 0;
        using var ticker = clock.CreateTimer(_ => Interlocked.Increment(ref count), null, TimeSpan.FromSeconds(1), TimeSpan.FromSeconds(1));

        await clock.AdvanceUntilAsync(() => Volatile.Read(ref count) >= 3, TimeSpan.FromSeconds(1), "three ticks");

        Assert.True(Volatile.Read(ref count) >= 3);
    }
}
