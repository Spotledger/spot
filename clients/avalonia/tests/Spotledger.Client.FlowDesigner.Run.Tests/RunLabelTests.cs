using System;
using System.Collections.Generic;
using System.IO;
using System.Text.RegularExpressions;
using Spotledger.Client.FlowDesigner;
using Spotledger.Client.FlowDesigner.Run;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// ADR-080: a person never reads the internal word for a run. Every name for a run comes from the
/// label pair <c>bpm.run.singular</c> / <c>bpm.run.plural</c> (default "Request"/"Requests"), and no
/// user-visible string — the catalogue or a literal in this project's own source — carries it.
public sealed class RunLabelTests
{

    [Fact]

    public void TheDefaultsAreTheDesignersLabelPair()
    {
        RunStrings strings = new();

        Assert.Equal("Request", strings.Labels.Singular);
        Assert.Equal("Requests", strings.Labels.Plural);
        Assert.Equal(RunLabels.SingularKey, "bpm.run.singular");
        Assert.Contains("requests", strings.NoRuns, StringComparison.OrdinalIgnoreCase);
        Assert.Equal("Requests", strings.Labels.Plural);
        Assert.Contains("request", strings.TestStarted("run_1"), StringComparison.Ordinal);
    }

    [Fact]

    public void AnOrganisationOverrideRenamesEveryStringThatNamesARun()
    {
        RunStrings strings = new(RunLabels.Resolve("Application", "Applications"));

        Assert.Equal("Applications of order_intake, version 3", strings.GroupHeading("order_intake", 3));
        Assert.Contains("Application run_7", strings.RunHeading(new RunSummary(
            "run_7", "order_intake", 3, RunOutcome.Succeeded, RunFixtures.T0)), StringComparison.Ordinal);
        Assert.Contains("No applications yet", strings.NoRuns, StringComparison.OrdinalIgnoreCase);
        Assert.DoesNotContain("request", strings.NoRuns, StringComparison.OrdinalIgnoreCase);
    }

    [Theory]
    [InlineData("Request", "Requests")]
    [InlineData("Application", "Applications")]
    [InlineData("Claim", "Claims")]

    public void NoUserVisibleStringCarriesTheInternalWord(string singular, string plural)
    {
        RunStrings strings = new(RunLabels.Resolve(singular, plural));

        foreach (string text in strings.All)
        {
            Assert.False(ContainsInternalWord(text), "the string names a run with the internal word: " + text);
        }
    }

    [Fact]

    public void NoStringLiteralInTheProjectCarriesTheInternalWord()
    {
        DirectoryInfo? project = FindProjectDirectory();
        Assert.NotNull(project);
        List<string> offenders = [];
        foreach (string file in Directory.EnumerateFiles(project!.FullName, "*.cs", SearchOption.AllDirectories))
        {
            if (file.Contains($"{Path.DirectorySeparatorChar}obj{Path.DirectorySeparatorChar}", StringComparison.Ordinal)
                || file.Contains($"{Path.DirectorySeparatorChar}bin{Path.DirectorySeparatorChar}", StringComparison.Ordinal))
            {
                continue;
            }

            foreach (string line in File.ReadLines(file))
            {
                string trimmed = line.TrimStart();
                if (trimmed.StartsWith("///", StringComparison.Ordinal)
                    || trimmed.StartsWith("//", StringComparison.Ordinal)
                    || trimmed.StartsWith('*'))
                {
                    continue;
                }

                foreach (Match match in Regex.Matches(line, "\"((?:[^\"\\\\]|\\\\.)*)\""))
                {
                    string literal = match.Groups[1].Value;
                    if (ContainsInternalWord(literal) && !LooksLikeCode(literal))
                    {
                        offenders.Add(Path.GetFileName(file) + ": " + literal);
                    }
                }
            }
        }

        Assert.Empty(offenders);
    }

    [Fact]

    public void EveryCatalogueStringIsNonEmpty()
    {
        RunStrings strings = new();

        Assert.True(strings.All.Count >= 40, "the catalogue covers every screen this task adds");
        Assert.All(strings.All, text => Assert.False(string.IsNullOrWhiteSpace(text)));
    }

    [Fact]

    public void TheDurationTextIsWholeUnits()
    {
        Assert.Equal("0 ms", RunStrings.DurationText(TimeSpan.Zero));
        Assert.Equal("820 ms", RunStrings.DurationText(TimeSpan.FromMilliseconds(820)));
        Assert.Equal("1.0 s", RunStrings.DurationText(TimeSpan.FromMilliseconds(1000)));
        Assert.Equal("1.2 s", RunStrings.DurationText(TimeSpan.FromMilliseconds(1249)));
        Assert.Equal("2 min 05 s", RunStrings.DurationText(TimeSpan.FromSeconds(125)));
    }

    private static bool ContainsInternalWord(string text) =>
        Regex.IsMatch(text, "\\bcase\\b", RegexOptions.IgnoreCase, TimeSpan.FromMilliseconds(200));

    // An error code or a contract key (lower-case dotted, e.g. bpm.run.something) keeps the internal
    // vocabulary by design: ADR-080 rule 1 asks for exactly that.
    private static bool LooksLikeCode(string literal) =>
        Regex.IsMatch(literal, "^[a-z][a-z0-9_.]*$", RegexOptions.None, TimeSpan.FromMilliseconds(200));

    private static DirectoryInfo? FindProjectDirectory()
    {
        DirectoryInfo? directory = new(AppContext.BaseDirectory);
        while (directory is not null)
        {
            string candidate = Path.Combine(directory.FullName, "src", "Spotledger.Client.FlowDesigner.Run");
            if (Directory.Exists(candidate))
            {
                return new DirectoryInfo(candidate);
            }

            directory = directory.Parent;
        }

        return null;
    }
}
