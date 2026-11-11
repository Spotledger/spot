using System.Globalization;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace Spotledger.Client.TestSupport;

public sealed record PerfBudgetEntry(string Id, string Metric, double Max, string Unit, string Runner);

/// <summary>Strict, dependency-free reader for the documented budget TOML subset.</summary>
public sealed partial class PerfBudget
{
    public IReadOnlyDictionary<string, PerfBudgetEntry> Entries { get; }
    public IReadOnlyDictionary<string, double> Multipliers { get; }
    public IReadOnlyDictionary<string, int> Workload { get; }

    private PerfBudget(Dictionary<string, PerfBudgetEntry> entries, Dictionary<string, double> multipliers,
        Dictionary<string, int> workload) => (Entries, Multipliers, Workload) = (entries, multipliers, workload);

    public static PerfBudget Load(string path) => Parse(File.ReadAllText(path));

    public static PerfBudget Parse(string text)
    {
        var budgets = new List<Dictionary<string, string>>();
        var runners = new Dictionary<string, Dictionary<string, string>>(StringComparer.Ordinal);
        var flat = new Dictionary<string, string>(StringComparer.Ordinal);
        var current = flat;
        foreach (var raw in text.Split('\n'))
        {
            // Comments outside quoted strings only. The subset uses JSON-compatible basic strings.
            var line = StripComment(raw).Trim();
            if (line.Length == 0) continue;
            if (line == "[[budget]]")
            {
                current = new(StringComparer.Ordinal);
                budgets.Add(current);
            }
            else if (RunnerHeader().Match(line) is { Success: true } match)
            {
                current = new(StringComparer.Ordinal);
                if (!runners.TryAdd(match.Groups[1].Value, current)) throw new FormatException("Duplicate runner table.");
            }
            else
            {
                var parts = line.Split('=', 2, StringSplitOptions.TrimEntries);
                if (parts.Length != 2 || !Key().IsMatch(parts[0]) || parts[1].Length == 0 ||
                    !current.TryAdd(parts[0], parts[1])) throw new FormatException($"Invalid or duplicate budget key: {line}");
            }
        }
        var entries = new Dictionary<string, PerfBudgetEntry>(StringComparer.Ordinal);
        var workload = new Dictionary<string, int>(StringComparer.Ordinal);
        if (flat.Count > 0)
        {
            if (budgets.Count > 0) throw new FormatException("Do not mix legacy and [[budget]] declarations.");
            RequireKeys(flat, ["max_frame_ms", "max_alloc_bytes_per_frame", "first_paint_50_rows_ms", "rows", "columns", "scroll_steps"]);
            entries.Add("grid.frame.max", new("grid.frame.max", "max", Number(flat["max_frame_ms"]), "ms", "reference"));
            entries.Add("grid.frame.alloc.mean", new("grid.frame.alloc.mean", "mean", Number(flat["max_alloc_bytes_per_frame"]), "bytes", "reference"));
            entries.Add("grid.first-paint", new("grid.first-paint", "p95", Number(flat["first_paint_50_rows_ms"]), "ms", "reference"));
            foreach (var key in new[] { "rows", "columns", "scroll_steps" })
            {
                if (!int.TryParse(flat[key], CultureInfo.InvariantCulture, out var value) || value <= 0)
                    throw new FormatException($"Invalid workload {key}.");
                workload.Add(key, value);
            }
        }
        foreach (var item in budgets)
        {
            RequireKeys(item, ["id", "metric", "max", "unit", "runner"]);
            var entry = new PerfBudgetEntry(String(item["id"]), String(item["metric"]), Number(item["max"]),
                String(item["unit"]), String(item["runner"]));
            if (!Identifier().IsMatch(entry.Id) || entry.Metric is not ("p50" or "p95" or "max" or "mean") ||
                entry.Unit is not ("ms" or "bytes") || entry.Runner is not ("reference" or "ci-linux" or "ci-windows") ||
                !entries.TryAdd(entry.Id, entry)) throw new FormatException($"Invalid or duplicate budget {entry.Id}.");
        }
        if (entries.Count == 0) throw new FormatException("No budgets declared.");
        var multipliers = new Dictionary<string, double>(StringComparer.Ordinal);
        foreach (var (label, item) in runners)
        {
            RequireKeys(item, ["multiplier"]);
            var value = Number(item["multiplier"]);
            if (value < 1 || (label == "reference" && value != 1)) throw new FormatException("Multipliers must be >= 1; reference must be 1.");
            multipliers.Add(label, value);
        }
        return new(entries, multipliers, workload);
    }

    public double Multiplier(string runner, string kind)
    {
        if (kind is not ("hosted" or "self-hosted" or "reference")) throw new ArgumentException("Unknown runner kind.", nameof(kind));
        if (kind == "reference")
        {
            if (runner != "reference") throw new ArgumentException("Reference kind requires the reference label.", nameof(runner));
            return 1;
        }
        if (runner == "reference") throw new ArgumentException("Reference label requires verified reference kind.", nameof(runner));
        return kind == "hosted" ? 1 : Multipliers.TryGetValue(runner, out var value) ? value : 3;
    }

    private static void RequireKeys(Dictionary<string, string> values, string[] keys)
    {
        if (values.Count != keys.Length || keys.Any(key => !values.ContainsKey(key)))
            throw new FormatException($"Expected exactly: {string.Join(", ", keys)}.");
    }

    private static double Number(string text)
    {
        if (!Numeric().IsMatch(text) || !double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out var value) ||
            !double.IsFinite(value) || value <= 0) throw new FormatException($"Invalid positive number: {text}");
        return value;
    }

    private static string String(string text)
    {
        try { return JsonSerializer.Deserialize<string>(text) ?? throw new FormatException("Null string."); }
        catch (JsonException error) { throw new FormatException("Expected a double-quoted basic string.", error); }
    }

    private static string StripComment(string text)
    {
        var quoted = false;
        var escaped = false;
        for (var i = 0; i < text.Length; i++)
        {
            var ch = text[i];
            if (ch == '"' && !escaped) quoted = !quoted;
            if (ch == '#' && !quoted) return text[..i];
            escaped = ch == '\\' && !escaped;
        }
        return text;
    }

    [GeneratedRegex(@"^\[runner\.([A-Za-z0-9_-]+)\]$")]
    private static partial Regex RunnerHeader();
    [GeneratedRegex(@"^[a-z_][a-z_0-9]*$")]
    private static partial Regex Key();
    [GeneratedRegex(@"^[A-Za-z0-9][A-Za-z0-9._-]*$")]
    private static partial Regex Identifier();
    [GeneratedRegex(@"^[0-9]+(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?$")]
    private static partial Regex Numeric();
}
