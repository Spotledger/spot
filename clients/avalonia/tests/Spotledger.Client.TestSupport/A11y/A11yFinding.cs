using System.Globalization;

namespace Spotledger.Client.TestSupport.A11y;

/// <summary>The accessibility rules the headless audit enforces (WP-UI-16.T2).</summary>
public static class A11yRule
{
    /// <summary>An interactive control exposes no accessible name: no name, no label association.</summary>
    public const string MissingName = "missing-name";
    /// <summary>A button whose only content is a graphic (icon) exposes no accessible name.</summary>
    public const string IconOnlyButton = "icon-only-button";
    /// <summary>Two siblings share the same accessible name and role, so neither can be told apart.</summary>
    public const string DuplicateSibling = "duplicate-sibling";
    /// <summary>A custom-drawn grid exposes no peer/rows/selection pattern to an assistive technology.</summary>
    public const string GridPatterns = "grid-patterns";
    /// <summary>The tab order contradicts the visual reading order (mirrored in RTL).</summary>
    public const string TabOrder = "tab-order";
    /// <summary>A focusable control paints no visible focus indicator.</summary>
    public const string FocusRing = "focus-ring";
    /// <summary>A text/background pair rendered below WCAG AA (4.5:1, or 3:1 for large text).</summary>
    public const string Contrast = "contrast";
    /// <summary>A transition lasts longer than the reduced-motion bound.</summary>
    public const string Motion = "motion";

    public static IReadOnlyList<string> All { get; } =
        [MissingName, IconOnlyButton, DuplicateSibling, GridPatterns, TabOrder, FocusRing, Contrast, Motion];
}

/// <summary>
/// One accessibility defect. The id is stable across runs — audit scope, rule, control type and
/// the automation id/path — so the ratchet baseline can only shrink; it never depends on order,
/// memory addresses or a message string. <see cref="Detail"/> is for humans only.
/// </summary>
public sealed record A11yFinding
{
    public A11yFinding(string scope, string rule, string control, string path, string detail)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(scope);
        ArgumentException.ThrowIfNullOrWhiteSpace(rule);
        ArgumentException.ThrowIfNullOrWhiteSpace(control);
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        (Scope, Rule, Control, Path, Detail) = (scope, rule, control, path, detail ?? string.Empty);
        Id = string.Create(CultureInfo.InvariantCulture, $"{scope}|{rule}|{control}|{path}");
    }

    public string Id { get; }
    public string Scope { get; }
    public string Rule { get; }
    public string Control { get; }
    public string Path { get; }
    public string Detail { get; }

    public override string ToString() => $"{Rule} {Control} {Path}: {Detail}";
}

/// <summary>The findings of one audit run, in a deterministic order.</summary>
public sealed record A11yReport(string Scope, IReadOnlyList<A11yFinding> Findings)
{
    public int Count => Findings.Count;

    public IReadOnlyList<A11yFinding> For(string rule) => Findings.Where(finding => finding.Rule == rule).ToList();

    /// <summary>One line per finding, plus a count per rule — what a failing test prints.</summary>
    public string Describe()
    {
        var lines = new List<string> { $"{Scope}: {Findings.Count} finding(s)" };
        foreach (var group in Findings.GroupBy(finding => finding.Rule).OrderBy(group => group.Key, StringComparer.Ordinal))
            lines.Add($"  {group.Key}: {group.Count()}");
        lines.AddRange(Findings.Select(finding => "  - " + finding));
        return string.Join(Environment.NewLine, lines);
    }
}

/// <summary>Which rules run, and how much pixel work they may do.</summary>
public sealed record A11yOptions
{
    /// <summary>Rules to run; the default is every rule.</summary>
    public IReadOnlySet<string> Rules { get; init; } = A11yRule.All.ToHashSet(StringComparer.Ordinal);

    /// <summary>Compare rendered pixels (focus ring, contrast). Needs a shown window with a real rasterizer.</summary>
    public bool Pixels { get; init; } = true;

    /// <summary>How many focusable controls get the focus-ring check; a window may hold hundreds.</summary>
    public int FocusRingSample { get; init; } = 16;

    /// <summary>Width in device-independent pixels of the ring compared with and without focus.</summary>
    public int RingWidth { get; init; } = 2;

    /// <summary>WCAG AA for ordinary text.</summary>
    public double MinContrast { get; init; } = 4.5;

    /// <summary>WCAG AA for large text (18.66 px bold, or 24 px) and UI components.</summary>
    public double MinLargeContrast { get; init; } = 3.0;

    /// <summary>Report transitions longer than the reduced-motion bound as motion findings.</summary>
    public bool CheckMotion { get; init; } = true;

    public bool Runs(string rule) => Rules.Contains(rule);
}
