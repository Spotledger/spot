using System.Text.Json;
using System.Text.Json.Serialization;

namespace Spotledger.Client.TestSupport.A11y;

/// <summary>One committed finding. The id is what the ratchet compares; the rest is for a reader.</summary>
public sealed record A11yBaselineEntry(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("rule")] string Rule,
    [property: JsonPropertyName("control")] string Control,
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("detail")] string Detail);

/// <summary>The committed `a11y-baseline.json`: today's accepted findings, one per line.</summary>
public sealed record A11yBaselineDocument(
    [property: JsonPropertyName("version")] int Version,
    [property: JsonPropertyName("findings")] IReadOnlyList<A11yBaselineEntry> Findings)
{
    public const int CurrentVersion = 1;
}

/// <summary>What changed against the baseline. <see cref="Added"/> fails; <see cref="Removed"/> must be deleted.</summary>
public sealed record A11yRatchet(IReadOnlyList<A11yFinding> Added, IReadOnlyList<string> Removed, int BaselineSize, int CurrentCount)
{
    public bool Clean => Added.Count == 0 && Removed.Count == 0;

    /// <summary>A message that tells a reader exactly what to do; empty when clean.</summary>
    public string Describe()
    {
        var lines = new List<string>();
        if (Added.Count > 0)
        {
            lines.Add($"{Added.Count} new accessibility finding(s) — fix them or record them deliberately:");
            lines.AddRange(Added.Select(finding => $"  + {finding}"));
        }
        if (Removed.Count > 0)
        {
            lines.Add($"{Removed.Count} baseline entr(y/ies) no longer match any finding — the file may only shrink, so delete them:");
            lines.AddRange(Removed.Select(id => "  - " + id));
        }
        return string.Join(Environment.NewLine, lines);
    }

    /// <summary>The size and the ten worst entries, for the PR body and the test log.</summary>
    public static string Summarise(A11yBaselineDocument document, int top = 10)
    {
        ArgumentNullException.ThrowIfNull(document);
        var byRule = document.Findings.GroupBy(entry => entry.Rule).OrderByDescending(group => group.Count()).ThenBy(group => group.Key, StringComparer.Ordinal);
        var lines = new List<string>
        {
            $"a11y baseline: {document.Findings.Count} finding(s) across {byRule.Count()} rule(s); it may only shrink.",
        };
        lines.AddRange(byRule.Select(group => $"  {group.Key}: {group.Count()}"));
        lines.Add($"top {Math.Min(top, document.Findings.Count)}:");
        lines.AddRange(document.Findings.Take(top).Select(entry => $"  {entry.Id} — {entry.Detail}"));
        return string.Join(Environment.NewLine, lines);
    }
}

/// <summary>Reads, writes and compares the accessibility ratchet baseline. JSON only, no new dependency.</summary>
public static class A11yBaseline
{
    /// <summary>Load the baseline; a missing or malformed file is a failure, never an empty baseline.</summary>
    public static A11yBaselineDocument Load(string path)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        if (!File.Exists(path)) throw new FileNotFoundException($"No accessibility baseline at {path}. Create it deliberately: {UpdateCommand}", path);
        A11yBaselineDocument? document;
        try
        {
            document = JsonSerializer.Deserialize<A11yBaselineDocument>(File.ReadAllText(path));
        }
        catch (JsonException error)
        {
            throw new InvalidDataException($"{path}: {error.Message}", error);
        }
        if (document is null) throw new InvalidDataException($"{path}: empty baseline document");
        if (document.Version != A11yBaselineDocument.CurrentVersion)
            throw new InvalidDataException($"{path}: version {document.Version}, expected {A11yBaselineDocument.CurrentVersion}");
        var duplicates = document.Findings.GroupBy(entry => entry.Id, StringComparer.Ordinal).Where(group => group.Count() > 1).Select(group => group.Key).ToList();
        if (duplicates.Count > 0) throw new InvalidDataException($"{path}: duplicate id(s) {string.Join(", ", duplicates)}");
        return document;
    }

    /// <summary>Write today's findings as the baseline. Only an explicit update run calls this.</summary>
    public static void Save(string path, IEnumerable<A11yFinding> findings)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        ArgumentNullException.ThrowIfNull(findings);
        var entries = findings
            .GroupBy(finding => finding.Id, StringComparer.Ordinal)
            .Select(group => group.First())
            .OrderBy(finding => finding.Id, StringComparer.Ordinal)
            .Select(finding => new A11yBaselineEntry(finding.Id, finding.Rule, finding.Control, finding.Path, finding.Detail))
            .ToList();
        var directory = Path.GetDirectoryName(Path.GetFullPath(path));
        if (!string.IsNullOrEmpty(directory)) Directory.CreateDirectory(directory);
        File.WriteAllText(path, Serialize(new A11yBaselineDocument(A11yBaselineDocument.CurrentVersion, entries)));
    }

    /// <summary>
    /// One finding per line: the baseline is reviewed in diffs, and an indented document would cost
    /// eight lines per entry (the PR line budget is real). Valid JSON, stable order.
    /// </summary>
    internal static string Serialize(A11yBaselineDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        var builder = new System.Text.StringBuilder();
        builder.Append("{\n  \"version\": ").Append(document.Version).Append(",\n  \"findings\": [");
        for (var index = 0; index < document.Findings.Count; index++)
        {
            var entry = document.Findings[index];
            builder.Append(index == 0 ? "\n" : ",\n").Append("    {");
            Append(builder, "id", entry.Id);
            builder.Append(", ");
            Append(builder, "rule", entry.Rule);
            builder.Append(", ");
            Append(builder, "control", entry.Control);
            builder.Append(", ");
            Append(builder, "path", entry.Path);
            builder.Append(", ");
            Append(builder, "detail", entry.Detail);
            builder.Append('}');
        }

        builder.Append(document.Findings.Count == 0 ? "]\n}\n" : "\n  ]\n}\n");
        return builder.ToString();
    }

    private static void Append(System.Text.StringBuilder builder, string name, string value) =>
        builder.Append('"').Append(name).Append("\": \"").Append(JsonEncodedText.Encode(value).ToString()).Append('"');

    /// <summary>Compare this run with the baseline: new ids are added, unknown ids must be deleted.</summary>
    public static A11yRatchet Compare(A11yBaselineDocument document, IEnumerable<A11yFinding> findings)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(findings);
        var known = document.Findings.Select(entry => entry.Id).ToHashSet(StringComparer.Ordinal);
        var current = findings.GroupBy(finding => finding.Id, StringComparer.Ordinal).Select(group => group.First()).ToList();
        var currentIds = current.Select(finding => finding.Id).ToHashSet(StringComparer.Ordinal);
        var added = current.Where(finding => !known.Contains(finding.Id)).OrderBy(finding => finding.Id, StringComparer.Ordinal).ToList();
        var removed = known.Where(id => !currentIds.Contains(id)).OrderBy(id => id, StringComparer.Ordinal).ToList();
        return new A11yRatchet(added, removed, known.Count, current.Count);
    }

    /// <summary>The command that writes the baseline; printed when it is missing or a test fails.</summary>
    public static string UpdateCommand =>
        "UPDATE_A11Y_BASELINE=1 dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests --filter FullyQualifiedName~GalleryAccessibility";

    /// <summary>True only when the caller explicitly asked to rewrite the baseline.</summary>
    public static bool UpdateRequested =>
        Environment.GetEnvironmentVariable("UPDATE_A11Y_BASELINE") == "1"
        || Environment.GetEnvironmentVariable("UPDATE_BASELINES") == "1";
}
