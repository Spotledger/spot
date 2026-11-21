using System.Diagnostics;

namespace Spotledger.Client.TestSupport;

/// <summary>Nearest-rank percentiles; no best-of-N masking. Inputs are copied and sorted.</summary>
public sealed record PerfStatistics(int Count, double P50, double P95, double Max, double Mean)
{
    public static PerfStatistics From(IEnumerable<double> values)
    {
        ArgumentNullException.ThrowIfNull(values);
        var sorted = values.Order().ToArray();
        if (sorted.Length == 0 || sorted.Any(value => !double.IsFinite(value) || value < 0))
            throw new ArgumentException("Samples must be nonempty, finite and nonnegative.", nameof(values));
        return new(sorted.Length, sorted[(int)Math.Ceiling(sorted.Length * .5) - 1],
            sorted[(int)Math.Ceiling(sorted.Length * .95) - 1], sorted[^1], sorted.Average());
    }

    public double Metric(string name) => name switch
    {
        "p50" => P50, "p95" => P95, "max" => Max, "mean" => Mean,
        _ => throw new ArgumentException("Unknown statistic.", nameof(name)),
    };
}

public sealed record PerfMeasurement(PerfStatistics WallTime, PerfStatistics Allocations)
{
    public double For(PerfBudgetEntry budget)
    {
        ArgumentNullException.ThrowIfNull(budget);
        return (budget.Unit == "ms" ? WallTime : Allocations).Metric(budget.Metric);
    }
}

public static class PerfMeter
{
    /// <summary>Synchronous operations only: async continuations invalidate per-thread allocation counts.</summary>
    public static PerfMeasurement Measure(Action operation, int samples = 20, int warmup = 3, Action<int>? prepare = null)
    {
        ArgumentNullException.ThrowIfNull(operation);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(samples);
        ArgumentOutOfRangeException.ThrowIfNegative(warmup);
        for (var i = -warmup; i < 0; i++)
        {
            prepare?.Invoke(i);
            operation();
        }
        var times = new double[samples];
        var allocations = new double[samples];
        for (var i = 0; i < samples; i++)
        {
            prepare?.Invoke(i);
            var before = GC.GetAllocatedBytesForCurrentThread();
            var start = Stopwatch.GetTimestamp();
            operation();
            var stop = Stopwatch.GetTimestamp();
            allocations[i] = GC.GetAllocatedBytesForCurrentThread() - before;
            times[i] = Stopwatch.GetElapsedTime(start, stop).TotalMilliseconds;
        }
        return new(PerfStatistics.From(times), PerfStatistics.From(allocations));
    }
}
