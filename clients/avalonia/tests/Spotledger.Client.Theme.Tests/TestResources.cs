using System.Globalization;
using System.Text.Json;

namespace Spotledger.Client.Theme.Tests;

/// <summary>One string from a test fixture: a stable id, the group it belongs to, and the text.</summary>
public sealed record UrduString(string Id, string Category, string Text);

/// <summary>
/// Loads the Urdu fixtures copied next to the test assembly. They are test data, not the
/// product's translation catalogue (that is a separate package, WP-UI-15's out-of-scope list).
/// </summary>
public static class TestResources
{
    private static readonly Lazy<IReadOnlyList<UrduString>> UrduSet = new(() => Load("UrduResourceSet.json"));
    private static readonly Lazy<IReadOnlyList<UrduString>> MixedSet = new(() => Load("MixedFieldStrings.json"));

    /// <summary>The pure-Urdu set the no-tofu check shapes (367 strings).</summary>
    public static IReadOnlyList<UrduString> Urdu => UrduSet.Value;

    /// <summary>Urdu with Latin codes, numbers, dates and phone numbers (20 strings).</summary>
    public static IReadOnlyList<UrduString> Mixed => MixedSet.Value;

    private static List<UrduString> Load(string file)
    {
        var path = Path.Combine(AppContext.BaseDirectory, "Resources", file);
        using var document = JsonDocument.Parse(File.ReadAllText(path));
        var strings = new List<UrduString>();
        foreach (var entry in document.RootElement.GetProperty("strings").EnumerateArray())
        {
            strings.Add(new UrduString(
                entry.GetProperty("id").GetString() ?? throw new InvalidOperationException($"{file}: an entry has no id."),
                entry.GetProperty("category").GetString() ?? string.Empty,
                entry.GetProperty("text").GetString() ?? string.Empty));
        }

        return strings;
    }

    /// <summary>An invariant-frequency Urdu culture; shaping must not depend on the OS culture.</summary>
    public static CultureInfo UrduCulture { get; } = CultureInfo.GetCultureInfo("ur-PK");

    private static readonly Lazy<string> Root = new(() =>
    {
        for (DirectoryInfo? directory = new(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
        {
            if (File.Exists(Path.Combine(directory.FullName, "clients", "avalonia", "design", "tokens.json")))
            {
                return directory.FullName;
            }
        }

        throw new DirectoryNotFoundException("Could not find the repository root from the test output directory.");
    });

    /// <summary>The repository root, found by walking up from the test assembly.</summary>
    public static string RepositoryRoot => Root.Value;
}
