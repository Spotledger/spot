using System.Text.RegularExpressions;
using Xunit;

namespace Spotledger.Client.Analytics.Tests;

/// <summary>
/// The shipped known-screen set is generated at build time from tools/ui-coverage/screens/*.toml:
/// the first test compares it with the repository manifest (both sides empty today, which still
/// has to hold), the second proves the generator reads a non-empty manifest (the fixture in this
/// project, built by the same inline task) and excludes example*.toml exactly as check.py does.
/// </summary>
public sealed partial class KnownScreensTests
{
    [Fact]
    public void The_generated_set_equals_the_repository_manifest()
    {
        var directory = FindManifestDirectory();
        var fromManifest = ReadIds(directory);

        Assert.Equal(fromManifest, KnownScreens.Manifest.Ids);
        Assert.Equal(fromManifest.Length, KnownScreens.Manifest.Count);
        Assert.DoesNotContain("example.invoice.form", KnownScreens.Manifest.Ids);
    }

    [Fact]
    public void The_generator_reads_a_non_empty_manifest_and_skips_examples()
    {
        // Fixtures/screens holds demo.toml (two screens) and example.toml (one, excluded).
        var ids = FixtureScreens.ManifestIds();

        Assert.Equal(["demo.invoice.form", "demo.invoice.list"], ids);
        Assert.DoesNotContain("fixture.example.only", ids);
    }

    [Fact]
    public void An_empty_set_rejects_every_screen_id()
    {
        var empty = KnownScreens.Empty;

        Assert.Equal(0, empty.Count);
        Assert.False(empty.Contains("a.b"));
        Assert.False(empty.Contains("sales.invoice.form"));
        Assert.False(empty.Contains(null));
        Assert.False(empty.Contains(string.Empty));
    }

    [Fact]
    public void From_rejects_ids_that_are_not_identifiers()
    {
        Assert.Throws<ArgumentException>(() => KnownScreens.From(["Not An Id"]));
        Assert.Throws<ArgumentException>(() => KnownScreens.From([new string('a', 65)]));
        Assert.Equal(0, KnownScreens.From([]).Count);
    }

    [Fact]
    public void The_generated_ids_are_all_identifier_shaped()
    {
        Assert.All(KnownScreens.Manifest.Ids, id => Assert.True(AnalyticsIds.IsValidId(id), id));
    }

    /// <summary>
    /// Same rule as tools/ui-coverage/check.py: every *.toml in the directory except example*
    /// (and the build fails on anything else it cannot parse).
    /// </summary>
    private static string[] ReadIds(DirectoryInfo directory)
    {
        var ids = new List<string>();
        foreach (var file in directory.GetFiles("*.toml").OrderBy(f => f.Name, StringComparer.Ordinal))
        {
            if (file.Name.StartsWith("example", StringComparison.Ordinal))
            {
                continue;
            }

            foreach (Match match in IdLine().Matches(File.ReadAllText(file.FullName)))
            {
                ids.Add(match.Groups[1].Value);
            }
        }

        ids.Sort(StringComparer.Ordinal);
        return [.. ids];
    }

    private static DirectoryInfo FindManifestDirectory()
    {
        var directory = new DirectoryInfo(AppContext.BaseDirectory);
        while (directory is not null)
        {
            var candidate = new DirectoryInfo(Path.Combine(directory.FullName, "tools", "ui-coverage", "screens"));
            if (candidate.Exists)
            {
                return candidate;
            }

            directory = directory.Parent;
        }

        throw new DirectoryNotFoundException("tools/ui-coverage/screens was not found above " + AppContext.BaseDirectory);
    }

    [GeneratedRegex("^id\\s*=\\s*\"([^\"]+)\"\\s*$", RegexOptions.Multiline)]
    private static partial Regex IdLine();
}
