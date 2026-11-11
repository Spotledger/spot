using Avalonia;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid.Gallery;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.TestSupport.A11y;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>
/// Reduced motion (WP-UI-16.T2). The theme exposes motion durations but no reduced-motion token, so
/// the bound lives in <see cref="ReducedMotion"/> (TestSupport only) and these tests prove that (a)
/// every catalogue duration is longer than the bound and is clamped to it, and (b) no control in the
/// galleries or the shell carries a transition that outlives it.
/// </summary>
public sealed class ReducedMotionTests
{
    private static readonly string[] MotionTokens = [Tokens.TokenMotionFast, Tokens.TokenMotionNormal, Tokens.TokenMotionSlow];

    [AvaloniaFact]
    public void EveryMotionTokenIsClampedToTheReducedMotionBound()
    {
        var application = Application.Current;
        Assert.NotNull(application);
        foreach (var key in MotionTokens)
        {
            Assert.True(SpotledgerTheme.TryGetToken(application!, key, ThemeVariantKind.Light, out var value), $"{key} is missing");
            var duration = Assert.IsType<TimeSpan>(value);
            Assert.InRange(duration, ReducedMotion.ReducedBound + TimeSpan.FromMilliseconds(1), TimeSpan.FromSeconds(5));
            Assert.Equal(duration, ReducedMotion.Clamp(duration));
            using (ReducedMotion.Enable())
                Assert.Equal(ReducedMotion.ReducedBound, ReducedMotion.Clamp(duration));
            Assert.Equal(duration, ReducedMotion.Clamp(duration));
        }
    }

    [AvaloniaFact]
    public void TheReducedMotionBoundIsShortEnoughToReadAsAStateChange()
    {
        Assert.Equal(TimeSpan.FromMilliseconds(100), ReducedMotion.ReducedBound);
        Assert.Equal(TimeSpan.MaxValue, ReducedMotion.Bound);
        using var first = ReducedMotion.Enable();
        Assert.True(ReducedMotion.IsEnabled);
        using (ReducedMotion.Enable())
        {
            Assert.True(ReducedMotion.IsEnabled);
            Assert.Equal(ReducedMotion.ReducedBound, ReducedMotion.Bound);
        }
        Assert.True(ReducedMotion.IsEnabled, "Nested leases must not end reduced motion early.");
        first.Dispose();
        Assert.False(ReducedMotion.IsEnabled);
    }

    [AvaloniaFact]
    public void NoGalleryOrShellTransitionOutlivesTheReducedMotionBound()
    {
        using var lease = ReducedMotion.Enable();
        var motionOnly = new A11yOptions { Pixels = false, Rules = new HashSet<string>([A11yRule.Motion]) };
        foreach (var (scope, content, width, height, variant, direction) in Scopes())
        {
            var window = A11yHost.Show(content(), width, height, variant, direction, scope);
            try
            {
                var report = AccessibilityAudit.Run(scope, window, motionOnly);
                Assert.True(report.Count == 0, report.Describe());
            }
            finally
            {
                A11yHost.Close(window);
            }
        }
    }

    private static IEnumerable<(string Scope, Func<Avalonia.Controls.Control> Content, int Width, int Height,
        Avalonia.Styling.ThemeVariant Variant, Avalonia.Media.FlowDirection Direction)> Scopes()
    {
        yield return ("catalogues-light-ltr", () => new ControlGallery(), 1000, 800,
            Avalonia.Styling.ThemeVariant.Light, Avalonia.Media.FlowDirection.LeftToRight);
        yield return ("grid-light-ltr", GridGalleryPage.BuildSample, 900, 600,
            Avalonia.Styling.ThemeVariant.Light, Avalonia.Media.FlowDirection.LeftToRight);
        yield return ("shell-light", () => new ShellView { DataContext = new ShellViewModel(new Spotledger.Client.Api.ApiConnection()) },
            1280, 800, Avalonia.Styling.ThemeVariant.Light, Avalonia.Media.FlowDirection.LeftToRight);
    }
}
