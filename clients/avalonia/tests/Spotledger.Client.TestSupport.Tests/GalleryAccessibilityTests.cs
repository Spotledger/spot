using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Styling;
using Spotledger.Client.Api;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid.Gallery;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.TestSupport.A11y;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>
/// The accessibility gate (WP-UI-16.T2) over every gallery and the shell chrome, in Light, Dark and
/// (when the palette is present) both high-contrast variants, LTR and RTL. It is a ratchet, not a
/// cliff: today's real findings are committed in `a11y-baseline.json`, only a new finding fails, and
/// the file may only shrink — a fixed finding is a failing test until its entry is deleted.
/// </summary>
public sealed class GalleryAccessibilityTests(ITestOutputHelper output)
{
    /// <summary>The committed baseline, next to this test project's other review artefacts.</summary>
    internal static string BaselinePath =>
        Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "..", "..", "..", "a11y-baseline.json"));

    private static readonly A11yOptions Options = new();

    [AvaloniaFact]
    public void EveryGalleryAndTheShellMatchTheAccessibilityBaseline()
    {
        var findings = new List<A11yFinding>();
        foreach (var (scope, content, width, height, variant, direction) in Scopes())
        {
            var window = A11yHost.Show(content(), width, height, variant, direction, scope);
            try
            {
                var report = AccessibilityAudit.Run(scope, window, Options);
                output.WriteLine($"{scope}: {report.Count} finding(s)");
                findings.AddRange(report.Findings);
            }
            finally
            {
                A11yHost.Close(window);
            }
        }

        if (A11yBaseline.UpdateRequested)
        {
            A11yBaseline.Save(BaselinePath, findings);
            output.WriteLine($"wrote {findings.Count} finding(s) to {BaselinePath}");
            output.WriteLine(A11yRatchet.Summarise(A11yBaseline.Load(BaselinePath)));
            return;
        }

        var document = A11yBaseline.Load(BaselinePath);
        output.WriteLine(A11yRatchet.Summarise(document));
        var ratchet = A11yBaseline.Compare(document, findings);
        Assert.True(ratchet.Clean,
            ratchet.Describe() + Environment.NewLine
            + $"baseline {BaselinePath}; rewrite it deliberately with {A11yBaseline.UpdateCommand}");
    }

    /// <summary>
    /// The four theme variants are all required. High contrast is skipped with a message when the
    /// palette is absent (WP-UI-15.T3 is a dependency, so today it is present) — never silently.
    /// </summary>
    [AvaloniaFact]
    public void EveryPlannedThemeVariantIsAvailable()
    {
        var missing = MissingHighContrast();
        if (missing.Count > 0)
            Assert.Skip($"high-contrast palette absent ({string.Join(", ", missing)}); those scopes were skipped, never silently audited as Light.");
    }

    private static List<string> MissingHighContrast()
    {
        var missing = new List<string>();
        if (!HighContrastAvailable(ThemeVariant.Light)) missing.Add("high-contrast");
        if (!HighContrastAvailable(ThemeVariant.Dark)) missing.Add("high-contrast-dark");
        return missing;
    }

    private static bool HighContrastAvailable(ThemeVariant variant)
    {
        var kind = variant == ThemeVariant.Dark ? ThemeVariantKind.HighContrastDark : ThemeVariantKind.HighContrast;
        return Application.Current is { } application
            && SpotledgerTheme.TryGetToken(application, Tokens.TokenColorTextPrimary, kind, out var value)
            && value is not null;
    }

    /// <summary>Every piece of chrome the audit owns, with its fixed host size.</summary>
    internal static IEnumerable<(string Scope, Func<Control> Content, int Width, int Height, ThemeVariant Variant, FlowDirection Direction)> Scopes()
    {
        foreach (var (name, variant) in Variants())
        {
            yield return ($"catalogues-{name}-ltr", () => new ControlGallery(), 1000, 800, variant, FlowDirection.LeftToRight);
            yield return ($"grid-{name}-ltr", GridGalleryPage.BuildSample, 900, 600, variant, FlowDirection.LeftToRight);
        }
        foreach (var (name, variant) in Variants())
            yield return ($"catalogues-{name}-rtl", () => new ControlGallery(), 1000, 800, variant, FlowDirection.RightToLeft);
        yield return ("grid-light-rtl", GridGalleryPage.BuildSample, 900, 600, ThemeVariant.Light, FlowDirection.RightToLeft);
        foreach (var (name, variant) in OrdinaryVariants())
            yield return ($"shell-{name}", () => new ShellView { DataContext = new ShellViewModel(new ApiConnection()) },
                1280, 800, variant, FlowDirection.LeftToRight);
    }

    private static IEnumerable<(string Name, ThemeVariant Variant)> OrdinaryVariants() =>
        [("light", ThemeVariant.Light), ("dark", ThemeVariant.Dark)];

    internal static IEnumerable<(string Name, ThemeVariant Variant)> Variants()
    {
        foreach (var pair in OrdinaryVariants()) yield return pair;
        if (HighContrastAvailable(ThemeVariant.Light)) yield return ("high-contrast", ThemeVariants.HighContrast);
        if (HighContrastAvailable(ThemeVariant.Dark)) yield return ("high-contrast-dark", ThemeVariants.HighContrastDark);
    }

    /// <summary>
    /// A control whose opacity runs a 300 ms transition when it is attached: the frames before the
    /// window settles differ, and the frame the audit reads does not (it is the final colour).
    /// </summary>
    [AvaloniaFact]
    public void ASettledWindowIsAuditedAfterItsOpacityTransitionEnded()
    {
        var box = new Border
        {
            Background = Brushes.Black,
            Opacity = 0,
            Transitions = [new Avalonia.Animation.DoubleTransition { Property = Visual.OpacityProperty, Duration = TimeSpan.FromMilliseconds(300) }],
        };
        box.AttachedToVisualTree += (_, _) => box.Opacity = 1;
        var window = A11yHost.Show(box, 100, 100);
        try
        {
            // The transition has finished by the time Show returns, and settling had to look at
            // more than one frame to know it (a 300 ms transition cannot end in the first frame).
            Assert.Equal(1.0, box.Opacity);
            Assert.True(A11yHost.Settle(window) >= 2);
        }
        finally
        {
            A11yHost.Close(window);
        }
    }

    /// <summary>A control that repaints a different colour every frame forever must fail loudly, naming what moved.</summary>
    [AvaloniaFact]
    public void AControlThatNeverSettlesFailsWithAClearMessage()
    {
        var error = Assert.Throws<InvalidOperationException>(() => A11yHost.Show(new Flicker(), 100, 100));
        Assert.Contains("never settled", error.Message, StringComparison.Ordinal);
        Assert.Contains("frame(s)", error.Message, StringComparison.Ordinal);
        Assert.Contains("Flicker", error.Message, StringComparison.Ordinal);
    }

    /// <summary>A spinner stand-in: every frame it draws schedules the next one in a different colour, forever.</summary>
    private sealed class Flicker : Control
    {
        private int _step;

        public override void Render(DrawingContext context)
        {
            var level = (byte)(_step * 37 % 256);
            context.DrawRectangle(new SolidColorBrush(Color.FromRgb(level, 0, 0)), null, new Rect(Bounds.Size));
            Avalonia.Threading.Dispatcher.UIThread.Post(() =>
            {
                _step++;
                InvalidateVisual();
            });
        }
    }
}
