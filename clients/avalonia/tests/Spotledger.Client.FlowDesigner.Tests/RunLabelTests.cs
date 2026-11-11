using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Text.RegularExpressions;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// ADR-080: a person never reads the internal word for a run. Every name for a run comes from the
/// label pair <c>bpm.run.singular</c> / <c>bpm.run.plural</c> (default "Request"/"Requests"), the
/// designer previews the resolved pair, and no user-visible string — the catalogue or a literal in
/// the project's own source — carries the internal word.
public sealed class RunLabelTests
{
    [Fact]

    public void TheDefaultsAreRequestAndRequests()
    {
        RunLabels labels = RunLabels.Defaults;

        Assert.Equal("Request", labels.Singular);
        Assert.Equal("Requests", labels.Plural);
        Assert.Equal("bpm.run.singular", RunLabels.SingularKey);
        Assert.Equal("bpm.run.plural", RunLabels.PluralKey);
        Assert.Equal("request", labels.SingularLower);
    }

    [Fact]

    public void AnOrganisationOverrideReplacesTheDefaultAndAMissingOneFallsBack()
    {
        RunLabels labels = RunLabels.Resolve("Application", null);

        Assert.Equal("Application", labels.Singular);
        Assert.Equal("Requests", labels.Plural);
        Assert.Throws<ArgumentException>(() => RunLabels.Resolve("  ", "Applications"));
    }

    [Fact]

    public void TheDesignerPreviewsTheResolvedPair()
    {
        FlowStrings defaults = new();
        FlowStrings school = new(RunLabels.Resolve("Application", "Applications"));

        Assert.Contains("Request", defaults.RunLabelPreview, StringComparison.Ordinal);
        Assert.Contains("Requests", defaults.RunLabelPreview, StringComparison.Ordinal);
        Assert.Contains("Application", school.RunLabelPreview, StringComparison.Ordinal);
        Assert.Contains("Applications", school.RunLabelPreview, StringComparison.Ordinal);
        Assert.DoesNotContain("Request", school.RunLabelPreview, StringComparison.Ordinal);
    }

    [Theory]
    [InlineData("Request", "Requests")]
    [InlineData("Application", "Applications")]
    [InlineData("Claim", "Claims")]

    public void NoUserVisibleStringCarriesTheInternalWord(string singular, string plural)
    {
        FlowStrings strings = new(RunLabels.Resolve(singular, plural));

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
        FlowStrings strings = new();

        Assert.True(strings.All.Count >= 40, "the catalogue covers every screen this task adds");
        Assert.All(strings.All, text => Assert.False(string.IsNullOrWhiteSpace(text)));
    }

    private static bool ContainsInternalWord(string text) =>
        Regex.IsMatch(text, "\\bcase\\b", RegexOptions.IgnoreCase, TimeSpan.FromMilliseconds(200));

    // An error code or a contract key (lower-case dotted, e.g. bpm.something.here) keeps the
    // internal term by design: ADR-080 rule 1 asks for exactly that.

    private static bool LooksLikeCode(string literal) =>
        Regex.IsMatch(literal, "^[a-z][a-z0-9_.]*$", RegexOptions.None, TimeSpan.FromMilliseconds(200));

    private static DirectoryInfo? FindProjectDirectory()
    {
        DirectoryInfo? directory = new(AppContext.BaseDirectory);
        while (directory is not null)
        {
            string candidate = Path.Combine(directory.FullName, "src", "Spotledger.Client.FlowDesigner");
            if (Directory.Exists(candidate))
            {
                return new DirectoryInfo(candidate);
            }

            directory = directory.Parent;
        }

        return null;
    }
}
