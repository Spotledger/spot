using System.Globalization;

namespace Spotledger.Client.TestSupport;

/// <summary>
/// How much slower than an idle machine this one is right now, for the local budget assertions of
/// the UI tests. A hand-written constant cannot know that ten runners share the host; the load
/// average can. The idle-machine budget does not change: load at or below the core count gives 1.
/// </summary>
public static class LoadScale
{
    /// <summary>The environment variable that sets the scale by hand (a positive number).</summary>
    public const string OverrideVariable = "SPOT_UI_BUDGET_SCALE";

    /// <summary>The highest scale the load average alone can produce.</summary>
    public const double MaxLoadScale = 4;

    private const string ProcLoadAvg = "/proc/loadavg";

    /// <summary>The load scale of this machine now.</summary>
    public static double Current =>
        From(ReadLoadAverage(ProcLoadAvg), Environment.ProcessorCount, Environment.GetEnvironmentVariable(OverrideVariable));

    /// <summary>The multiplier a local budget assertion applies: CI keeps its constant 3, times the load scale.</summary>
    public static double Applied => (Environment.GetEnvironmentVariable("CI") is null ? 1 : 3) * Current;

    /// <summary>The text a failure message appends, so a reader sees the scale and where it came from.</summary>
    public static string Describe()
    {
        var load = ReadLoadAverage(ProcLoadAvg);
        var source = Environment.GetEnvironmentVariable(OverrideVariable) is { Length: > 0 } value
            ? $"{OverrideVariable}={value}"
            : load is { } l ? $"loadavg1={l.ToString("F2", CultureInfo.InvariantCulture)} on {Environment.ProcessorCount} core(s)" : "no load average";
        return $"scale {Applied.ToString("F2", CultureInfo.InvariantCulture)} ({source}, CI={(Environment.GetEnvironmentVariable("CI") is null ? "no" : "yes")})";
    }

    /// <summary>The pure rule. An override wins; otherwise <c>clamp(load / processors, 1, 4)</c>; no load gives 1.</summary>
    /// <param name="loadAverage">The one-minute load average, or null when it cannot be read.</param>
    /// <param name="processors">The core count.</param>
    /// <param name="overrideValue">The value of <see cref="OverrideVariable"/>, or null when unset.</param>
    public static double From(double? loadAverage, int processors, string? overrideValue)
    {
        if (!string.IsNullOrWhiteSpace(overrideValue))
        {
            if (!double.TryParse(overrideValue, NumberStyles.Float, CultureInfo.InvariantCulture, out var parsed)
                || !double.IsFinite(parsed) || parsed <= 0)
            {
                throw new ArgumentException(
                    $"{OverrideVariable} must be a positive number, got '{overrideValue}'.", nameof(overrideValue));
            }

            return parsed;
        }

        if (loadAverage is not { } load || !double.IsFinite(load) || processors <= 0) return 1;
        return Math.Clamp(load / processors, 1, MaxLoadScale);
    }

    /// <summary>Reads the first field of a <c>/proc/loadavg</c>-shaped file; null when missing or unreadable.</summary>
    public static double? ReadLoadAverage(string path)
    {
        try
        {
            if (!File.Exists(path)) return null;
            var first = File.ReadAllText(path).Split(' ', StringSplitOptions.RemoveEmptyEntries).FirstOrDefault();
            return double.TryParse(first, NumberStyles.Float, CultureInfo.InvariantCulture, out var value) ? value : null;
        }
        catch (IOException)
        {
            return null;
        }
        catch (UnauthorizedAccessException)
        {
            return null;
        }
    }
}
