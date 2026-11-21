using System.Globalization;
using System.Text.Json;

namespace Spotledger.Client.TestSupport;

public sealed class BudgetExceededException(string message) : Exception(message);

/// <summary>Writes one atomic JSON object per assertion, including breaches. Never overwrites another sample.</summary>
public sealed class Budget
{
    private readonly PerfBudget _budgets;
    private readonly string _runner;
    private readonly string _kind;
    private readonly string? _output;
    private readonly string _commit;
    private readonly bool _advisory;

    public Budget(PerfBudget budgets, string? runner = null, string? kind = null, string? output = null, string? commit = null, bool? advisory = null)
    {
        ArgumentNullException.ThrowIfNull(budgets);
        _budgets = budgets;
        _runner = runner ?? Environment.GetEnvironmentVariable("SPOT_PERF_RUNNER") ?? (OperatingSystem.IsWindows() ? "ci-windows" : "ci-linux");
        _kind = kind ?? Environment.GetEnvironmentVariable("SPOT_RUNNER_KIND") ?? "self-hosted";
        _output = output ?? Environment.GetEnvironmentVariable("SPOT_PERF_OUT");
        _commit = commit ?? Environment.GetEnvironmentVariable("GITHUB_SHA") ?? "local";
        // Opt-in: a breach is recorded as a WARNING instead of failing. For runners whose speed the
        // budgets were never calibrated for (shared hosted VMs); the default stays strict.
        _advisory = advisory ?? Environment.GetEnvironmentVariable("SPOT_PERF_ADVISORY") == "1";
    }

    public void Assert(string id, double measured) => AssertCore(id, measured, null);

    public void Assert(string id, PerfMeasurement measurement)
    {
        ArgumentNullException.ThrowIfNull(measurement);
        if (!_budgets.Entries.TryGetValue(id, out var entry)) throw new ArgumentException($"Unknown budget {id}.", nameof(id));
        AssertCore(id, measurement.For(entry), measurement);
    }

    private void AssertCore(string id, double measured, PerfMeasurement? samples)
    {
        if (!double.IsFinite(measured) || measured < 0) throw new ArgumentOutOfRangeException(nameof(measured));
        if (!_budgets.Entries.TryGetValue(id, out var entry)) throw new ArgumentException($"Unknown budget {id}.", nameof(id));
        var multiplier = _budgets.Multiplier(_runner, _kind);
        var allowed = entry.Max * multiplier;
        var breach = measured > allowed;
        var status = !breach ? "PASS" : multiplier > 1 || _advisory ? "WARNING" : "FAIL";
        var record = new { id, measured, max = entry.Max, runner = _runner, commit = _commit,
            metric = entry.Metric, unit = entry.Unit, multiplier, allowed, status, samples };
        if (!string.IsNullOrEmpty(_output))
        {
            Directory.CreateDirectory(_output);
            var path = Path.Combine(_output, Guid.NewGuid().ToString("N", CultureInfo.InvariantCulture) + ".json");
            var temporary = path + ".tmp";
            File.WriteAllText(temporary, JsonSerializer.Serialize(record));
            File.Move(temporary, path);
        }
        var message = FormattableString.Invariant($"{status}: {id} measured {measured} {entry.Unit}; allowed {allowed} (base {entry.Max}, runner {_runner}, APPLIED MULTIPLIER {multiplier}).");
        Console.WriteLine(message);
        if (status == "FAIL") throw new BudgetExceededException(message);
    }
}
