using System.Globalization;
using System.Text.Json;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

public sealed class PerfTests
{
    private const string Declaration = """
        [[budget]]
        id = "paint"
        metric = "p95"
        max = 10
        unit = "ms"
        runner = "reference"
        [runner.ci-linux]
        multiplier = 4
        """;

    [Theory]
    [InlineData("", "No budgets")]
    [InlineData("[[budget]]\nid = \"x\"", "Expected exactly")]
    [InlineData("max_frame_ms = 2", "Expected exactly")]
    [InlineData("[[nope]]", "Invalid")]
    public void LoaderFailsClosed(string text, string message)
    {
        Assert.Contains(message, Assert.Throws<FormatException>(() => PerfBudget.Parse(text)).Message);
    }

    [Theory]
    [InlineData("max = 10", "max = nan")]
    [InlineData("max = 10", "max = -1")]
    [InlineData("metric = \"p95\"", "metric = \"best\"")]
    [InlineData("unit = \"ms\"", "unit = \"seconds\"")]
    [InlineData("runner = \"reference\"", "runner = \"typo\"")]
    [InlineData("id = \"paint\"", "id = \"../paint\"")]
    [InlineData("multiplier = 4", "multiplier = 0.5")]
    public void InvalidDeclarationIsRejected(string oldValue, string newValue) =>
        Assert.Throws<FormatException>(() => PerfBudget.Parse(Declaration.Replace(oldValue, newValue, StringComparison.Ordinal)));

    [Fact]
    public void ScalingUsesExplicitTableAndSafeDefaults()
    {
        var budgets = PerfBudget.Parse(Declaration);
        Assert.Equal(4, budgets.Multiplier("ci-linux", "self-hosted"));
        Assert.Equal(1, budgets.Multiplier("ci-linux", "hosted"));
        Assert.Equal(3, budgets.Multiplier("unlabelled", "self-hosted"));
        Assert.Equal(1, budgets.Multiplier("unlabelled", "hosted"));
        Assert.Equal(1, budgets.Multiplier("reference", "reference"));
        Assert.Throws<ArgumentException>(() => budgets.Multiplier("reference", "self-hosted"));
        using var temp = new TemporaryDirectory();
        new Budget(budgets, "ci-linux", "self-hosted", temp.Path).Assert("paint", 100);
        using var json = JsonDocument.Parse(File.ReadAllText(Assert.Single(Directory.GetFiles(temp.Path, "*.json"))));
        Assert.Equal("WARNING", json.RootElement.GetProperty("status").GetString());
        Assert.Equal(40, json.RootElement.GetProperty("allowed").GetDouble());
    }

    [Fact]
    public void AdvisoryRecordsABreachAsAWarningAndStrictStillFails()
    {
        var budgets = PerfBudget.Parse(Declaration);
        using var temp = new TemporaryDirectory();
        Assert.Throws<BudgetExceededException>(() => new Budget(budgets, "ci-linux", "hosted", temp.Path, advisory: false).Assert("paint", 100));
        new Budget(budgets, "ci-linux", "hosted", temp.Path, advisory: true).Assert("paint", 100);
        var statuses = Directory.GetFiles(temp.Path, "*.json")
            .Select(file => JsonDocument.Parse(File.ReadAllText(file)).RootElement.GetProperty("status").GetString() ?? "")
            .OrderBy(status => status, StringComparer.Ordinal).ToArray();
        Assert.Equal(["FAIL", "WARNING"], statuses);
    }

    [Fact]
    public void PreparationIsOutsideMeasurementAndWarmupIsSeparate()
    {
        var prepared = new List<int>();
        var measurement = PerfMeter.Measure(() => { }, samples: 4, warmup: 2,
            prepare: index => { prepared.Add(index); GC.KeepAlive(new byte[10000]); });
        Assert.Equal([-2, -1, 0, 1, 2, 3], prepared);
        Assert.Equal(0, measurement.Allocations.Max);
    }

    [Fact]
    public void WarmupIsDiscardedAndSynchronousAllocationsAreMeasured()
    {
        var calls = 0;
        var samples = PerfMeter.Measure(() => { calls++; GC.KeepAlive(new byte[1024]); }, samples: 20, warmup: 3);
        Assert.Equal(23, calls);
        Assert.Equal(20, samples.WallTime.Count);
        Assert.True(samples.WallTime.P50 <= samples.WallTime.P95);
        Assert.True(samples.WallTime.P95 <= samples.WallTime.Max);
        Assert.True(samples.Allocations.Mean >= 1024);
        var stats = PerfStatistics.From([9, 1, 4, 2]);
        Assert.Equal(2, stats.P50);
        Assert.Equal(9, stats.P95);
        Assert.Equal(4, stats.Mean);
        Assert.Throws<ArgumentOutOfRangeException>(() => PerfMeter.Measure(() => { }, samples: 0));
        Assert.Throws<ArgumentException>(() => PerfStatistics.From([]));
    }

    [Fact]
    public void StatisticsAreIncludedInJsonEvidence()
    {
        using var temp = new TemporaryDirectory();
        var stats = PerfStatistics.From([1, 2, 3, 4]);
        var measurement = new PerfMeasurement(stats, stats);
        new Budget(PerfBudget.Parse(Declaration), "reference", "reference", temp.Path).Assert("paint", measurement);
        using var json = JsonDocument.Parse(File.ReadAllText(Assert.Single(Directory.GetFiles(temp.Path, "*.json"))));
        Assert.Equal(4, json.RootElement.GetProperty("samples").GetProperty("WallTime").GetProperty("Count").GetInt32());
        Assert.Equal(4, json.RootElement.GetProperty("measured").GetDouble());
    }

    [Fact]
    public void DuplicateAndUnknownBudgetsFailAndEqualityIsAllowed()
    {
        using var temp = new TemporaryDirectory();
        var budgets = PerfBudget.Parse(Declaration);
        var assertion = new Budget(budgets, "reference", "reference", temp.Path);
        assertion.Assert("paint", 10);
        assertion.Assert("paint", 10);
        Assert.Equal(2, Directory.GetFiles(temp.Path, "*.json").Length);
        Assert.Throws<ArgumentException>(() => assertion.Assert("unknown", 1));
        Assert.Throws<ArgumentOutOfRangeException>(() => assertion.Assert("paint", double.NaN));
        Assert.Throws<FormatException>(() => PerfBudget.Parse(Declaration + "\n[[budget]]\nid = \"paint\"\nmetric = \"p95\"\nmax = 1\nunit = \"ms\"\nrunner = \"reference\""));
        Assert.Throws<FormatException>(() => PerfBudget.Parse(Declaration + "\n[runner.ci-linux]\nmultiplier = 3"));
    }

    [Fact]
    public void BreachWritesEvidenceBeforeThrowing()
    {
        using var temp = new TemporaryDirectory();
        var budgets = PerfBudget.Parse(Declaration);
        var budget = new Budget(budgets, "reference", "reference", temp.Path, "abc");
        var error = Assert.Throws<BudgetExceededException>(() => budget.Assert("paint", 11));
        Assert.Contains("11", error.Message);
        Assert.Contains("10", error.Message);
        using var json = JsonDocument.Parse(File.ReadAllText(Assert.Single(Directory.GetFiles(temp.Path, "*.json"))));
        Assert.Equal("paint", json.RootElement.GetProperty("id").GetString());
        Assert.Equal(11, json.RootElement.GetProperty("measured").GetDouble());
        Assert.Equal("abc", json.RootElement.GetProperty("commit").GetString());
    }
}

internal sealed class TemporaryDirectory : IDisposable
{
    public string Path { get; } = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "spot-ui-" + Guid.NewGuid().ToString("N", CultureInfo.InvariantCulture));
    public TemporaryDirectory() => Directory.CreateDirectory(Path);
    public void Dispose() => Directory.Delete(Path, true);
}
