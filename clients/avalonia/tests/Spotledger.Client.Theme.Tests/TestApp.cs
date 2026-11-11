using Avalonia;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Theme.Tests.ThemeTestAppFactory))]

// One headless Avalonia platform per process: serialize test collections so the boot and
// window creation never race (the Avalonia headless template's default).
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The headless test application: the theme stack a real host installs — the Spotledger token
/// resources, the bundled Urdu faces and the Controls library ControlThemes — and nothing
/// else. No Fluent theme, so a token that only exists in a built-in theme cannot hide.
/// </summary>
public sealed class ThemeTestApp : Application
{
    public override void Initialize()
    {
        SpotledgerTheme.MergeTokenResources(this);
        SpotledgerFonts.Register();
        Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml"),
        });

        // A text-bearing baseline pins its fonts (docs/testing.md). The product token prefers
        // Inter/Segoe UI, and whatever a Linux host substitutes for those changes every glyph in
        // the screenshot — which is why baselines generated on one machine failed on CI by a few
        // hundred pixels. The test app therefore resolves the Latin family to the bundled Noto
        // Sans, so a baseline is a picture of *our* layout, not of the runner's font package.
        Resources["Font.Family.Latin"] = SpotledgerFonts.Latin;

        // Warm the bundled family once, before any test renders: the first shape resolves the
        // collection and caches the face metrics, and a page measured before that lands uses the
        // fallback family's line heights — which silently shifts every screenshot below it by a
        // few pixels and makes a baseline depend on test order.
        TestWarmUp.WarmUpUrduShaping();
    }
}

/// <summary>Font and shaping warm-up shared by the app factory and the screenshot helper.</summary>
public static class TestWarmUp
{
    private static readonly Lock Gate = new();
    private static Avalonia.Media.FontManager? _warmedWith;

    /// <summary>Resolves the Urdu family and shapes one Urdu line, once per process.</summary>
    public static void WarmUpUrduShaping()
    {
        lock (Gate)
        {
            if (ReferenceEquals(_warmedWith, Avalonia.Media.FontManager.Current))
            {
                return;
            }

            SpotledgerFonts.Register();
            var glyphTypeface = SpotledgerFonts.ResolveUrduGlyphTypeface();
            if (glyphTypeface is not null)
            {
                using var buffer = Avalonia.Media.TextFormatting.TextShaper.Current.ShapeText(
                    "اردو متن",
                    new Avalonia.Media.TextFormatting.TextShaperOptions(
                        glyphTypeface, 16, 1, TestResources.UrduCulture, 0, 0, []));
                _ = buffer.Length;
            }

            var warm = new Avalonia.Controls.TextBlock { Text = "اردو متن", FontFamily = SpotledgerFonts.Urdu, FontSize = 16 };
            warm.Measure(new Avalonia.Size(double.PositiveInfinity, double.PositiveInfinity));
            var latin = new Avalonia.Controls.TextBlock { Text = "1,234.50", FontFamily = SpotledgerFonts.Latin, FontSize = 16 };
            latin.Measure(new Avalonia.Size(double.PositiveInfinity, double.PositiveInfinity));
            _warmedWith = Avalonia.Media.FontManager.Current;
        }
    }
}

/// <summary>Builds the headless application with Skia drawing and HarfBuzz shaping.</summary>
public static class ThemeTestAppFactory
{
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<ThemeTestApp>()
            .UseSkia()
            .UseHarfBuzz()
            .UseHeadless(new AvaloniaHeadlessPlatformOptions
            {
                // A real rasterizer: CaptureRenderedFrame returns null with headless drawing,
                // and the no-tofu check needs the real glyph pipeline.
                UseHeadlessDrawing = false,
                OverlayPopups = false,
            });
}
