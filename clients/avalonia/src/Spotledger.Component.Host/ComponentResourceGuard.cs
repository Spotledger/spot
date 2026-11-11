using System.Diagnostics;

namespace Spotledger.Component.Host;

/// <summary>
/// A monotonic tick source, injected so nothing in the host reads a clock of its own. Time is an
/// argument everywhere in this package; a guard that measured with <c>DateTime.UtcNow</c> would be
/// untestable and would read the wall clock, which the platform forbids in logic.
/// </summary>
public interface IMonotonicClock
{
    /// <summary>The current tick count. Never goes backwards.</summary>
    long Timestamp { get; }
}

/// <summary>The machine's stopwatch as a monotonic clock: what the desktop shell passes.</summary>
public sealed class StopwatchClock : IMonotonicClock
{
    /// <summary>The single instance.</summary>
    public static StopwatchClock Instance { get; } = new();

    /// <inheritdoc />
    public long Timestamp => Stopwatch.GetTimestamp();
}

/// <summary>
/// What one guarded call did: its result, how long it took, and whether it overran its deadline.
/// </summary>
/// <typeparam name="T">The call's result type.</typeparam>
/// <param name="Value">The result, when the call produced one.</param>
/// <param name="ElapsedMilliseconds">The measured duration in whole milliseconds (rounded up).</param>
/// <param name="Refusal">The overrun, when the call exceeded its budget.</param>
public sealed record GuardedOutcome<T>(T? Value, long ElapsedMilliseconds, ComponentRefusal? Refusal)
{
    /// <summary>True when the call stayed inside its budget.</summary>
    public bool WithinBudget => Refusal is null;
}

/// <summary>
/// The resource guard around a component's constructor and its value updates: it measures each call
/// against an injected deadline and records a diagnostic when the call overruns, so a slow component
/// is visible in support logs and can be refused at install time. Budgets and measurements are whole
/// milliseconds — there is no floating point in a time comparison here.
/// <para>
/// <strong>This is a guard, not a sandbox.</strong> A synchronous constructor or update cannot be
/// preempted from managed code without a second process: the call is <em>measured</em>, and an
/// overrunning one is reported and (for a constructor) discarded. A component that blocks a thread
/// for a minute still blocks it for a minute; only a separate rendering process would change that,
/// and that is out of scope here.
/// </para>
/// </summary>
public sealed class ComponentResourceGuard
{
    private readonly IMonotonicClock _clock;
    private readonly IComponentHostLog _log;

    /// <summary>Creates a guard with its budgets and its clock.</summary>
    /// <param name="clock">The monotonic clock to measure with.</param>
    /// <param name="constructorBudget">The most a constructor may take.</param>
    /// <param name="updateBudget">The most one value update may take.</param>
    /// <param name="log">Where an overrun is recorded.</param>
    public ComponentResourceGuard(
        IMonotonicClock clock,
        TimeSpan constructorBudget,
        TimeSpan updateBudget,
        IComponentHostLog? log = null)
    {
        _clock = clock ?? throw new ArgumentNullException(nameof(clock));
        if (constructorBudget <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(constructorBudget), "The constructor budget is positive.");
        }

        if (updateBudget <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(updateBudget), "The update budget is positive.");
        }

        ConstructorBudget = constructorBudget;
        UpdateBudget = updateBudget;
        _log = log ?? NullComponentHostLog.Instance;
    }

    /// <summary>The most a constructor may take.</summary>
    public TimeSpan ConstructorBudget { get; }

    /// <summary>The most one value update may take.</summary>
    public TimeSpan UpdateBudget { get; }

    /// <summary>
    /// Runs a constructor within <see cref="ConstructorBudget"/>. An overrun is reported with
    /// <see cref="ComponentRefusalCodes.ConstructorBudgetExceeded"/> and the caller must discard the
    /// value; the component never reaches a slot.
    /// </summary>
    /// <typeparam name="T">The constructed type.</typeparam>
    /// <param name="componentId">The component, for the diagnostic.</param>
    /// <param name="create">The constructor call.</param>
    public GuardedOutcome<T> RunConstructor<T>(string componentId, Func<T> create)
    {
        ArgumentNullException.ThrowIfNull(create);
        var (value, elapsedTicks) = Measure(create);
        if (elapsedTicks <= ConstructorBudget.Ticks)
        {
            return new GuardedOutcome<T>(value, Milliseconds(elapsedTicks), null);
        }

        var refusal = new ComponentRefusal(
            ComponentRefusalCodes.ConstructorBudgetExceeded,
            "the constructor of '" + componentId + "' took " + Milliseconds(elapsedTicks) + " ms; the budget is "
            + Milliseconds(ConstructorBudget.Ticks) + " ms. The component was not put in the slot.");
        _log.Failed(refusal);
        return new GuardedOutcome<T>(default, Milliseconds(elapsedTicks), refusal);
    }

    /// <summary>
    /// Runs one value update within <see cref="UpdateBudget"/>. The update has already happened when
    /// an overrun is reported — managed code cannot abort it — so the outcome carries the diagnostic
    /// and the caller keeps the value it now holds.
    /// </summary>
    /// <param name="componentId">The component, for the diagnostic.</param>
    /// <param name="update">The update call.</param>
    public GuardedOutcome<bool> RunUpdate(string componentId, Action update)
    {
        ArgumentNullException.ThrowIfNull(update);
        var (_, elapsedTicks) = Measure(() =>
        {
            update();
            return true;
        });
        if (elapsedTicks <= UpdateBudget.Ticks)
        {
            return new GuardedOutcome<bool>(true, Milliseconds(elapsedTicks), null);
        }

        var refusal = new ComponentRefusal(
            ComponentRefusalCodes.UpdateBudgetExceeded,
            "one update of '" + componentId + "' took " + Milliseconds(elapsedTicks) + " ms; the budget is "
            + Milliseconds(UpdateBudget.Ticks) + " ms. The update stands and the overrun is logged.");
        _log.Failed(refusal);
        return new GuardedOutcome<bool>(true, Milliseconds(elapsedTicks), refusal);
    }

    /// <summary>Whole milliseconds, rounded up, without touching a floating point type.</summary>
    /// <param name="ticks">A tick count.</param>
    public static long Milliseconds(long ticks) =>
        ticks <= 0 ? 0 : (ticks + TimeSpan.TicksPerMillisecond - 1) / TimeSpan.TicksPerMillisecond;

    private (T Value, long ElapsedTicks) Measure<T>(Func<T> call)
    {
        var started = _clock.Timestamp;
        var value = call();
        return (value, _clock.Timestamp - started);
    }
}
