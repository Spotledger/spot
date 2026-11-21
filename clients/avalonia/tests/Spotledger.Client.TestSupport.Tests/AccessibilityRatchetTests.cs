using System.Text.Json;
using Spotledger.Client.TestSupport.A11y;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>
/// The ratchet (WP-UI-16.T2): it fails on a new finding, and it fails when the baseline still
/// lists something that was fixed, so the file can only shrink. A missing or malformed baseline is
/// a failure too — an absent baseline must never read as "no findings".
/// </summary>
public sealed class AccessibilityRatchetTests
{
    private static readonly string[] BothIds = ["gallery|missing-name|TextBox|/a", "gallery|missing-name|TextBox|/b"];
    private static readonly string[] RemovedSecond = ["gallery|missing-name|TextBox|/b"];

    private static A11yFinding Finding(string path, string rule = A11yRule.MissingName, string scope = "gallery") =>
        new(scope, rule, "TextBox", path, "TextBox has no accessible name");

    private static string TempFile()
    {
        var directory = Path.Combine(Path.GetTempPath(), "spot-a11y-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        return Path.Combine(directory, "a11y-baseline.json");
    }

    [Fact]
    public void SaveAndLoadRoundTripKeepsIdsAndOrder()
    {
        var path = TempFile();
        A11yBaseline.Save(path, [Finding("/b"), Finding("/a")]);
        var document = A11yBaseline.Load(path);
        Assert.Equal(A11yBaselineDocument.CurrentVersion, document.Version);
        Assert.Equal(BothIds, document.Findings.Select(entry => entry.Id));
        // One finding per line: the baseline is read in diffs, not as a blob.
        Assert.Equal(document.Findings.Count + 5, File.ReadAllLines(path).Length);
        Assert.True(A11yBaseline.Compare(document, [Finding("/a"), Finding("/b")]).Clean);
    }

    [Fact]
    public void ANewFindingFailsAndIsListed()
    {
        var document = A11yBaseline.Load(WriteAndLoad([Finding("/a")], out _));
        var ratchet = A11yBaseline.Compare(document, [Finding("/a"), Finding("/b")]);
        Assert.False(ratchet.Clean);
        Assert.Single(ratchet.Added);
        Assert.Equal("gallery|missing-name|TextBox|/b", ratchet.Added[0].Id);
        Assert.Empty(ratchet.Removed);
        Assert.Contains("1 new accessibility finding", ratchet.Describe());
        Assert.Contains("+ missing-name", ratchet.Describe());
    }

    [Fact]
    public void AFixedFindingMustBeDeletedFromTheBaseline()
    {
        var document = A11yBaseline.Load(WriteAndLoad([Finding("/a"), Finding("/b")], out _));
        var ratchet = A11yBaseline.Compare(document, [Finding("/a")]);
        Assert.False(ratchet.Clean);
        Assert.Empty(ratchet.Added);
        Assert.Equal(RemovedSecond, ratchet.Removed);
        Assert.Contains("delete them", ratchet.Describe());
    }

    [Fact]
    public void AChangeOfRuleOrScopeIsANewFindingNotAReinterpretation()
    {
        var document = A11yBaseline.Load(WriteAndLoad([Finding("/a")], out _));
        var otherScope = A11yBaseline.Compare(document, [Finding("/a", scope: "grid")]);
        Assert.Single(otherScope.Added);
        Assert.Single(otherScope.Removed);
        var otherRule = A11yBaseline.Compare(document, [Finding("/a", rule: A11yRule.FocusRing)]);
        Assert.Single(otherRule.Added);
        Assert.Single(otherRule.Removed);
    }

    [Fact]
    public void AMissingBaselineIsAFailureWithTheUpdateCommand()
    {
        var path = Path.Combine(Path.GetTempPath(), "spot-a11y-absent-" + Guid.NewGuid().ToString("N"), "a11y-baseline.json");
        var error = Assert.Throws<FileNotFoundException>(() => A11yBaseline.Load(path));
        Assert.Contains("UPDATE_A11Y_BASELINE=1 dotnet test", error.Message);
        Assert.Contains("GalleryAccessibility", error.Message);
    }

    [Fact]
    public void AMalformedOrForeignBaselineIsAFailure()
    {
        var path = TempFile();
        File.WriteAllText(path, "{ not json");
        Assert.Throws<InvalidDataException>(() => A11yBaseline.Load(path));
        File.WriteAllText(path, JsonSerializer.Serialize(new { version = 2, findings = Array.Empty<object>() }));
        Assert.Throws<InvalidDataException>(() => A11yBaseline.Load(path));
        File.WriteAllText(path, JsonSerializer.Serialize(new
        {
            version = 1,
            findings = new[]
            {
                new { id = "a", rule = "x", control = "y", path = "/z", detail = "" },
                new { id = "a", rule = "x", control = "y", path = "/z", detail = "" },
            },
        }));
        var error = Assert.Throws<InvalidDataException>(() => A11yBaseline.Load(path));
        Assert.Contains("duplicate id", error.Message);
    }

    [Fact]
    public void TheSummaryReportsTheSizeAndTheTopTen()
    {
        var path = WriteAndLoad(Enumerable.Range(0, 14).Select(index => Finding("/c" + index.ToString("00", System.Globalization.CultureInfo.InvariantCulture))), out var findings);
        var document = A11yBaseline.Load(path);
        var summary = A11yRatchet.Summarise(document);
        Assert.Contains("14 finding(s)", summary);
        Assert.Contains("top 10:", summary);
        Assert.Equal(10, summary.Split('\n').Count(line => line.TrimStart().StartsWith("gallery|", StringComparison.Ordinal)));
        Assert.Equal(findings.Count, document.Findings.Count);
    }

    [Fact]
    public void DuplicatesInARunCollapseToOneEntry()
    {
        var path = TempFile();
        A11yBaseline.Save(path, [Finding("/a"), Finding("/a")]);
        Assert.Single(A11yBaseline.Load(path).Findings);
    }

    private static string WriteAndLoad(IEnumerable<A11yFinding> findings, out List<A11yFinding> saved)
    {
        saved = findings.ToList();
        var path = TempFile();
        A11yBaseline.Save(path, saved);
        return path;
    }
}
