using Avalonia;
using Avalonia.Animation;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Styling;
using Spotledger.Client.TestSupport.A11y;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>
/// One failing and one passing fixture per audit rule (WP-UI-16.T2). The passing half is the part
/// that matters: a rule that fires on everything would look like a strict gate and be worthless.
/// </summary>
public sealed class AccessibilityAuditTests
{
    private static A11yReport Audit(Control content, A11yOptions options, FlowDirection direction = FlowDirection.LeftToRight)
    {
        var window = A11yHost.Show(content, 320, 240, ThemeVariant.Light, direction);
        try
        {
            return AccessibilityAudit.Run("fixture", window, options);
        }
        finally
        {
            A11yHost.Close(window);
        }
    }

    private static A11yOptions Only(string rule, bool pixels = false, int focusRingSample = 4) => new()
    {
        Rules = new HashSet<string>(StringComparer.Ordinal) { rule },
        Pixels = pixels,
        FocusRingSample = focusRingSample,
    };

    [AvaloniaFact]
    public void AnUnnamedTextBoxIsReportedAndANamedOneIsNot()
    {
        var failing = Audit(AccessibilityFixtures.TextBox(named: false), Only(A11yRule.MissingName));
        var finding = Assert.Single(failing.Findings);
        Assert.Equal(A11yRule.MissingName, finding.Rule);
        Assert.Contains("TextBox", finding.Control);
        Assert.Contains("no accessible name", finding.Detail);

        var passing = Audit(AccessibilityFixtures.TextBox(named: true), Only(A11yRule.MissingName));
        Assert.Empty(passing.Findings);
    }

    [AvaloniaFact]
    public void ALabeledByAssociationCountsAsAName()
    {
        Assert.Empty(Audit(AccessibilityFixtures.LabeledTextBox(linked: true), Only(A11yRule.MissingName)).Findings);
        Assert.Single(Audit(AccessibilityFixtures.LabeledTextBox(linked: false), Only(A11yRule.MissingName)).Findings);
    }

    [AvaloniaFact]
    public void AnIconOnlyButtonIsReportedOnceByTheSpecificRule()
    {
        // Both rules enabled: the icon-only defect is reported by its own rule, not twice.
        var options = new A11yOptions { Rules = new HashSet<string>([A11yRule.IconOnlyButton, A11yRule.MissingName]), Pixels = false };
        var failing = Audit(AccessibilityFixtures.IconButton(named: false), options);
        var finding = Assert.Single(failing.Findings);
        Assert.Equal(A11yRule.IconOnlyButton, finding.Rule);

        Assert.Empty(Audit(AccessibilityFixtures.IconButton(named: true), options).Findings);
    }

    [AvaloniaFact]
    public void DuplicateSiblingNamesAreReportedOncePerExtraSibling()
    {
        var failing = Audit(AccessibilityFixtures.Buttons(distinct: false), Only(A11yRule.DuplicateSibling));
        var finding = Assert.Single(failing.Findings);
        Assert.Equal(A11yRule.DuplicateSibling, finding.Rule);
        Assert.Contains("2 siblings", finding.Detail);

        Assert.Empty(Audit(AccessibilityFixtures.Buttons(distinct: true), Only(A11yRule.DuplicateSibling)).Findings);
    }

    [AvaloniaFact]
    public void ACustomDrawnGridWithoutAPeerIsReported()
    {
        var failing = Audit(AccessibilityFixtures.Grid(accessible: false), Only(A11yRule.GridPatterns));
        var finding = Assert.Single(failing.Findings);
        Assert.Equal(A11yRule.GridPatterns, finding.Rule);
        Assert.Contains("no grid automation peer", finding.Detail);

        var passing = Audit(AccessibilityFixtures.Grid(accessible: true), Only(A11yRule.GridPatterns));
        Assert.Empty(passing.Findings);
    }

    [AvaloniaFact]
    public void TabOrderThatContradictsReadingOrderIsReported()
    {
        var failing = Audit(AccessibilityFixtures.TabOrder(inverted: true), Only(A11yRule.TabOrder));
        var finding = Assert.Single(failing.Findings);
        Assert.Equal(A11yRule.TabOrder, finding.Rule);
        Assert.Contains("draw", finding.Detail);

        Assert.Empty(Audit(AccessibilityFixtures.TabOrder(inverted: false), Only(A11yRule.TabOrder)).Findings);
    }

    [AvaloniaFact]
    public void TabOrderIsMirroredInRightToLeft()
    {
        // In RTL the first child is drawn on the right, so the same TabIndex swap inverts the
        // relationship: what passes in LTR must fail in RTL and the other way round.
        Assert.Empty(Audit(AccessibilityFixtures.TabOrder(inverted: false, FlowDirection.RightToLeft),
            Only(A11yRule.TabOrder), FlowDirection.RightToLeft).Findings);
        Assert.Single(Audit(AccessibilityFixtures.TabOrder(inverted: true, FlowDirection.RightToLeft),
            Only(A11yRule.TabOrder), FlowDirection.RightToLeft).Findings);
        Assert.Single(Audit(AccessibilityFixtures.TabOrder(inverted: true), Only(A11yRule.TabOrder)).Findings);
    }

    [AvaloniaFact]
    public void AFocusedControlWithoutAVisibleIndicatorIsReported()
    {
        var failing = Audit(AccessibilityFixtures.FocusRing(indicates: false), Only(A11yRule.FocusRing, pixels: true));
        var finding = Assert.Single(failing.Findings);
        Assert.Equal(A11yRule.FocusRing, finding.Rule);
        Assert.Contains("2 px ring", finding.Detail);

        Assert.Empty(Audit(AccessibilityFixtures.FocusRing(indicates: true), Only(A11yRule.FocusRing, pixels: true)).Findings);
    }

    [AvaloniaFact]
    public void LowContrastTextIsReportedFromRenderedPixels()
    {
        var failing = Audit(AccessibilityFixtures.Contrast(passing: false), Only(A11yRule.Contrast, pixels: true));
        var finding = Assert.Single(failing.Findings);
        Assert.Equal(A11yRule.Contrast, finding.Rule);
        Assert.Contains("below 4.5:1", finding.Detail);

        Assert.Empty(Audit(AccessibilityFixtures.Contrast(passing: true), Only(A11yRule.Contrast, pixels: true)).Findings);
    }

    [AvaloniaFact]
    public void LargeTextUsesTheThreeToOneFloor()
    {
        // #767676 on white is 4.48:1: a failure as ordinary text, acceptable as large text.
        Assert.Empty(Audit(AccessibilityFixtures.LargeTextContrast(), Only(A11yRule.Contrast, pixels: true)).Findings);
        var ordinary = Audit(AccessibilityFixtures.Contrast(passing: false), Only(A11yRule.Contrast, pixels: true));
        Assert.NotEmpty(ordinary.Findings);
    }

    [AvaloniaFact]
    public void ATransitionLongerThanTheReducedMotionBoundIsReported()
    {
        var slow = new Border
        {
            Width = 100,
            Height = 40,
            Background = Brushes.WhiteSmoke,
            Transitions = new Transitions { new DoubleTransition { Property = Visual.OpacityProperty, Duration = TimeSpan.FromMilliseconds(400) } },
        };
        Assert.Empty(Audit(slow, Only(A11yRule.Motion)).Findings);
        using (ReducedMotion.Enable())
        {
            var finding = Assert.Single(Audit(slow, Only(A11yRule.Motion)).Findings);
            Assert.Equal(A11yRule.Motion, finding.Rule);
            Assert.Contains("400 ms", finding.Detail);
        }

        Assert.False(ReducedMotion.IsEnabled);
    }

    [AvaloniaFact]
    public void TheReportIsDeterministicAndFreeOfDuplicateIds()
    {
        var first = Audit(AccessibilityFixtures.Buttons(distinct: false), Only(A11yRule.DuplicateSibling));
        var second = Audit(AccessibilityFixtures.Buttons(distinct: false), Only(A11yRule.DuplicateSibling));
        Assert.Equal(first.Findings.Select(finding => finding.Id), second.Findings.Select(finding => finding.Id));
        Assert.Equal(first.Findings.Select(finding => finding.Id).Distinct().Count(), first.Findings.Count);
        Assert.Contains("duplicate-sibling", first.Describe());
        Assert.Contains("fixture:", first.Describe());
    }

    [AvaloniaFact]
    public void AnUnshownWindowIsRejectedRatherThanAuditedBlind()
    {
        var window = new Window { Content = new TextBlock { Text = "not shown" } };
        Assert.Throws<ArgumentException>(() => AccessibilityAudit.Run("fixture", window));
    }

    [AvaloniaFact]
    public void AnEmptyScopeIsRejected()
    {
        var window = new Window();
        Assert.Throws<ArgumentException>(() => AccessibilityAudit.Run(" ", window));
    }
}
