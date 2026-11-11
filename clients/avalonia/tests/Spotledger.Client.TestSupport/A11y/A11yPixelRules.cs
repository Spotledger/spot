using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Media;

namespace Spotledger.Client.TestSupport.A11y;

/// <summary>
/// The rules that judge rendered pixels: is the text readable on what is actually behind it, and
/// does a focused control change anything a person can see. Both work from a captured frame, so
/// they hold for every theme variant without reading token tables.
/// </summary>
internal static class A11yPixelRules
{
    /// <summary>Antialiasing may shift a channel this much without counting as a visible change.</summary>
    private const int ChannelDelta = 8;

    /// <summary>A colour must cover at least this many pixels inside a text rect to count as a glyph.</summary>
    private const int MinimumTextPixels = 4;

    public static void Contrast(IReadOnlyList<Control> controls, PixelBuffer frame, string scope, A11yOptions options,
        List<A11yFinding> findings, Visual pathRoot, Visual geometryRoot)
    {
        if (!options.Runs(A11yRule.Contrast)) return;
        foreach (var text in controls.OfType<TextBlock>())
        {
            if (!A11yTree.IsVisibleAndEnabled(text) || string.IsNullOrWhiteSpace(text.Text)) continue;
            if (A11yTree.BoundsIn(text, geometryRoot) is not { } bounds) continue;
            var rect = frame.Clip(bounds);
            if (rect.IsEmpty || rect.Width < 3 || rect.Height < 3) continue;
            var pair = SampleTextPair(frame, rect, DeclaredForeground(text));
            if (pair is null) continue;
            var large = IsLargeText(text);
            var minimum = large ? options.MinLargeContrast : options.MinContrast;
            if (pair.Value.Ratio + 0.005 >= minimum) continue;
            findings.Add(new(scope, A11yRule.Contrast, nameof(TextBlock), A11yTree.Path(text, pathRoot),
                $"{pair.Value.Ratio:0.00}:1 is below {minimum:0.0}:1 ({(large ? "large" : "ordinary")} text, "
                + $"#{pair.Value.Text.R:X2}{pair.Value.Text.G:X2}{pair.Value.Text.B:X2} on "
                + $"#{pair.Value.Background.R:X2}{pair.Value.Background.G:X2}{pair.Value.Background.B:X2}): \"{Shorten(text.Text)}\"."));
        }
    }

    /// <summary>The colour the control asked for; used to identify the glyph core among antialiasing.</summary>
    private static Color? DeclaredForeground(TextBlock text) =>
        text.Foreground is ISolidColorBrush { Color.A: > 0 } brush ? brush.Color : null;

    /// <summary>WCAG "large text": 24 px, or 18.66 px when bold or heavier.</summary>
    private static bool IsLargeText(TextBlock text) =>
        text.FontSize >= 24 || (text.FontSize >= 18.66 && text.FontWeight >= FontWeight.SemiBold);

    /// <summary>
    /// The background is the most common colour actually rendered in the text's rectangle (so an
    /// opacified or layered surface is judged as drawn). The text colour is the declared foreground
    /// when at least one pixel confirms it, otherwise the colour furthest from the background that
    /// covers enough pixels to be a glyph rather than antialiasing noise.
    /// </summary>
    internal static (PixelBuffer.Rgba8 Text, PixelBuffer.Rgba8 Background, double Ratio)? SampleTextPair(
        PixelBuffer frame, PixelRect rect, Color? declared = null)
    {
        var counts = new Dictionary<int, int>(256);
        var colours = new Dictionary<int, PixelBuffer.Rgba8>(256);
        for (var y = rect.Y; y < rect.Y + rect.Height; y++)
            for (var x = rect.X; x < rect.X + rect.Width; x++)
            {
                var colour = frame.At(x, y);
                counts[colour.Key] = counts.GetValueOrDefault(colour.Key) + 1;
                colours[colour.Key] = colour;
            }
        if (counts.Count == 0) return null;
        var backgroundKey = counts.MaxBy(entry => entry.Value).Key;
        var background = colours[backgroundKey];
        var backgroundLuminance = PixelBuffer.RelativeLuminance(background);
        PixelBuffer.Rgba8? text = null;
        if (declared is { } requested)
        {
            var key = (int)(requested.R << 24 | requested.G << 16 | requested.B << 8 | requested.A);
            text = colours.TryGetValue(key, out var exact) ? exact
                : counts.Where(entry => entry.Key != backgroundKey)
                    .Where(entry => ChannelClose(colours[entry.Key], requested))
                    .Select(entry => (PixelBuffer.Rgba8?)colours[entry.Key])
                    .FirstOrDefault();
        }
        if (text is null)
        {
            var extreme = counts
                .Where(entry => entry.Value >= MinimumTextPixels && entry.Key != backgroundKey)
                .OrderByDescending(entry => Math.Abs(PixelBuffer.RelativeLuminance(colours[entry.Key]) - backgroundLuminance))
                .Select(entry => (int?)entry.Key)
                .FirstOrDefault();
            if (extreme is null) return null;
            text = colours[extreme.Value];
        }
        var chosen = text.Value;
        if (chosen.Key == backgroundKey) return null;
        var composited = PixelBuffer.Composite(chosen, background);
        return (chosen, background, PixelBuffer.ContrastRatio(composited, background));
    }

    private static bool ChannelClose(PixelBuffer.Rgba8 colour, Color requested) =>
        Math.Abs(colour.R - requested.R) <= ChannelDelta
        && Math.Abs(colour.G - requested.G) <= ChannelDelta
        && Math.Abs(colour.B - requested.B) <= ChannelDelta;

    /// <summary>
    /// Focus a deterministic sample of controls and count the pixels that change in the
    /// <paramref name="ringWidth"/>-wide band hugging each control's edge (inside and just outside,
    /// so either ring style counts). No changed pixel means no visible focus indicator.
    /// </summary>
    public static void FocusRing(Window window, IReadOnlyList<Control> controls, string scope, A11yOptions options,
        List<A11yFinding> findings, Visual pathRoot, Visual geometryRoot)
    {
        if (!options.Runs(A11yRule.FocusRing)) return;
        var sample = controls
            .Where(control => !ReferenceEquals(control, pathRoot))
            .Where(control => control.Focusable && control.IsTabStop && A11yTree.IsVisibleAndEnabled(control))
            .OrderBy(control => A11yTree.Path(control, pathRoot), StringComparer.Ordinal)
            .ThenBy(control => control.GetType().Name, StringComparer.Ordinal)
            .Take(options.FocusRingSample)
            .ToList();
        if (sample.Count == 0) return;
        window.Focus();
        ClearFocus(window);
        var plain = PixelBuffer.Of(window);
        foreach (var control in sample)
        {
            if (A11yTree.BoundsIn(control, geometryRoot) is not { } bounds) continue;
            var region = RingRegion(bounds, options.RingWidth, plain);
            if (region.IsEmpty) continue;
            // Keyboard navigation, so :focus-visible styles apply exactly as they do for a Tab press.
            if (!control.Focus(NavigationMethod.Tab)) continue;
            var focused = PixelBuffer.Of(window);
            if (CountChanged(plain, focused, region, bounds, options.RingWidth) == 0)
                findings.Add(new(scope, A11yRule.FocusRing, control.GetType().Name, A11yTree.Path(control, pathRoot),
                    $"{A11yTree.Describe(control)} paints nothing in the {options.RingWidth} px ring around its edge when focused."));
            ClearFocus(window);
        }
    }

    private static void ClearFocus(TopLevel window)
    {
        if (window.FocusManager is { } manager) manager.Focus(null, NavigationMethod.Unspecified, KeyModifiers.None);
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
    }

    /// <summary>The pixels whose distance to the control's edge is at most <paramref name="width"/>, inside or outside.</summary>
    internal static PixelRect RingRegion(Rect bounds, int width, PixelBuffer frame)
    {
        var outer = frame.Clip(bounds.Inflate(width));
        return outer;
    }

    /// <summary>Count pixels in the ring band (outer rect minus the inset rect) that visibly changed.</summary>
    internal static int CountChanged(PixelBuffer before, PixelBuffer after, PixelRect outer, Rect bounds, int width)
    {
        var changed = 0;
        for (var y = outer.Y; y < outer.Y + outer.Height; y++)
            for (var x = outer.X; x < outer.X + outer.Width; x++)
            {
                // Pixel centres, so a fractional control edge does not shift the band by a pixel.
                var inside = x + 0.5 > bounds.X + width && x + 0.5 < bounds.Right - width
                    && y + 0.5 > bounds.Y + width && y + 0.5 < bounds.Bottom - width;
                if (inside) continue;
                if (!before.Contains(x, y) || !after.Contains(x, y)) continue;
                if (Differs(before.At(x, y), after.At(x, y))) changed++;
            }
        return changed;
    }

    internal static bool Differs(PixelBuffer.Rgba8 first, PixelBuffer.Rgba8 second) =>
        Math.Abs(first.R - second.R) > ChannelDelta
        || Math.Abs(first.G - second.G) > ChannelDelta
        || Math.Abs(first.B - second.B) > ChannelDelta
        || Math.Abs(first.A - second.A) > ChannelDelta;

    private static string Shorten(string text) => text.Length <= 40 ? text.ReplaceLineEndings(" ") : text[..40].ReplaceLineEndings(" ") + "…";
}
