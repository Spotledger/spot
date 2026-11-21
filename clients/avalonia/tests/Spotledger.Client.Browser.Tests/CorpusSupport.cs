using System.Text.Json;

namespace Spotledger.Client.Browser.Tests;

/// <summary>One case of the expression conformance corpus (<c>crates/framework/spot-expr/corpus</c>).</summary>
internal sealed record CorpusCase(string File, string Id, string RequestJson, string ExpectJson);

/// <summary>Finds and loads the corpus from the repository by walking up from the test assembly.</summary>
internal static class CorpusSupport
{
    private static readonly Lazy<Dictionary<string, CorpusCase>> Cases = new(Load);

    public static IEnumerable<string> Files() =>
        Cases.Value.Values.Select(c => c.File).Distinct(StringComparer.Ordinal).OrderBy(f => f, StringComparer.Ordinal);

    public static IReadOnlyList<CorpusCase> OfFile(string file) =>
        Cases.Value.Values.Where(c => c.File == file).OrderBy(c => c.Id, StringComparer.Ordinal).ToList();

    public static int Count => Cases.Value.Count;

    private static string CorpusDir()
    {
        var directory = new DirectoryInfo(AppContext.BaseDirectory);
        while (directory is not null)
        {
            var candidate = Path.Combine(directory.FullName, "crates", "framework", "spot-expr", "corpus");
            if (Directory.Exists(candidate) && Directory.Exists(Path.Combine(directory.FullName, "clients")))
            {
                return candidate;
            }

            directory = directory.Parent;
        }

        throw new InvalidOperationException("the conformance corpus is not found relative to the test assembly");
    }

    private static Dictionary<string, CorpusCase> Load()
    {
        var cases = new Dictionary<string, CorpusCase>(StringComparer.Ordinal);
        foreach (var file in Directory.GetFiles(CorpusDir(), "*.json").OrderBy(f => f, StringComparer.Ordinal))
        {
            using var document = JsonDocument.Parse(File.ReadAllText(file));
            var name = Path.GetFileName(file);
            foreach (var element in document.RootElement.EnumerateArray())
            {
                var id = element.GetProperty("id").GetString() ?? "?";
                // A corpus case already has the request shape (expr, context, now); the outcome
                // shape is its "expect" ({"ok": typed} or {"error": code[, "offset": n]}).
                var request = $"{{\"expr\":{JsonSerializer.Serialize(element.GetProperty("expr").GetString())},"
                    + $"\"now\":{element.GetProperty("now").GetRawText()},\"context\":{element.GetProperty("context").GetRawText()}}}";
                cases.Add(name + "|" + id, new CorpusCase(name, id, request, element.GetProperty("expect").GetRawText()));
            }
        }

        return cases;
    }
}
