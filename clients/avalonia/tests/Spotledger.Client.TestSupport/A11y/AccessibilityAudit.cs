using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.Threading;
using Avalonia.VisualTree;

namespace Spotledger.Client.TestSupport.A11y;

/// <summary>
/// The headless accessibility audit (WP-UI-16.T2). Walks a shown window's visual tree and rendered
/// pixels and reports defects instead of throwing, so one broken control never hides the rest.
/// Nothing is fixed here: the ratchet baseline records today's findings and only new ones fail.
/// </summary>
public static class AccessibilityAudit
{
    /// <summary>
    /// Audit <paramref name="window"/>. <paramref name="scope"/> names the audit (for example
    /// `control-gallery-light-ltr`) and is part of every finding id, so the same control in two
    /// theme variants is two independent baseline entries.
    /// </summary>
    public static A11yReport Run(string scope, Window window, A11yOptions? options = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(scope);
        ArgumentNullException.ThrowIfNull(window);
        options ??= new A11yOptions();
        if (!window.IsVisible)
            throw new ArgumentException("Show the window before auditing: the focus-ring and contrast rules read rendered pixels.", nameof(window));
        Dispatcher.UIThread.VerifyAccess();
        var controls = A11yTree.Descendants(window);
        // Paths start at the audited content, not at the window chrome: a template change in the
        // window host must not rewrite every baseline id.
        var pathRoot = window.Content as Visual ?? window;
        var findings = new List<A11yFinding>();
        A11yTreeRules.Name(controls, scope, options, findings, pathRoot);
        A11yTreeRules.DuplicateSiblings(controls, scope, options, findings, pathRoot, window);
        A11yTreeRules.GridPatterns(controls, scope, options, findings, pathRoot);
        A11yTreeRules.TabOrder(controls, scope, options, findings, pathRoot, window);
        A11yTreeRules.Motion(A11yTreeRules.Visuals(window), scope, options, findings, pathRoot);
        if (options.Pixels)
        {
            var frame = PixelBuffer.Of(window);
            A11yPixelRules.Contrast(controls, frame, scope, options, findings, pathRoot, window);
            A11yPixelRules.FocusRing(window, controls, scope, options, findings, pathRoot, window);
        }
        // One entry per id: a duplicate would make the ratchet ambiguous.
        var unique = findings.GroupBy(finding => finding.Id, StringComparer.Ordinal)
            .Select(group => group.First())
            .OrderBy(finding => finding.Id, StringComparer.Ordinal)
            .ToList();
        return new A11yReport(scope, unique);
    }
}

/// <summary>
/// Builds the fixed-size headless host an audit needs. The theme stack (Fluent, token dictionary,
/// control themes) is the one the test application installs, so the audit judges the real render.
/// </summary>
public static class A11yHost
{
    /// <summary>The longest dispatcher time <see cref="Settle"/> will spend waiting for a frame to stop changing.</summary>
    public static readonly TimeSpan SettleBudget = TimeSpan.FromSeconds(2);

    /// <summary>The most frames <see cref="Settle"/> captures before it gives up.</summary>
    public const int SettleMaxFrames = 60;

    /// <summary>Real time between two compared frames: a clock-driven transition moves visibly in this long.</summary>
    private static readonly TimeSpan SettleSpacing = TimeSpan.FromMilliseconds(20);

    /// <summary>Show <paramref name="content"/> in an undecorated window and settle it before it is audited.</summary>
    public static Window Show(Control content, int width, int height, ThemeVariant? variant = null,
        FlowDirection direction = FlowDirection.LeftToRight, string? title = null)
    {
        ArgumentNullException.ThrowIfNull(content);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(width);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(height);
        var window = new Window
        {
            Width = width,
            Height = height,
            CanResize = false,
            WindowDecorations = WindowDecorations.None,
            RequestedThemeVariant = variant ?? ThemeVariant.Light,
            FlowDirection = direction,
            Title = title ?? "a11y host",
            Content = content,
        };
        // Grayscale antialiasing, whatever the host's rasterizer prefers. Avalonia picks LCD
        // (coloured fringe) text on an opaque surface by default, and which pixels carry the exact
        // glyph colour then depends on the system FreeType/HarfBuzz the native Skia loads; the
        // contrast rule reads those pixels, so the frame it judges must not depend on them.
        TextOptions.SetTextRenderingMode(window, TextRenderingMode.Antialias);
        window.Show();
        Pump();
        try
        {
            Settle(window);
        }
        catch
        {
            Close(window);
            throw;
        }

        return window;
    }

    /// <summary>
    /// Captures frames, with the dispatcher pumping between them, until two consecutive frames
    /// <see cref="SettleSpacing"/> apart are pixel-identical. Avalonia transitions run on a clock, not
    /// on frame count, so an audit of the first frame would depend on how long the machine took.
    /// Bounded by <see cref="SettleBudget"/> and <see cref="SettleMaxFrames"/>; a frame that never
    /// settles fails naming the frame count, the first differing rectangle and the control there.
    /// </summary>
    /// <returns>The number of frames captured.</returns>
    public static int Settle(Window window)
    {
        ArgumentNullException.ThrowIfNull(window);
        var watch = System.Diagnostics.Stopwatch.StartNew();
        PixelBuffer? previous = null;
        Rect? lastDiff = null;
        for (var frames = 1; ; frames++)
        {
            var frame = PixelBuffer.Of(window);
            if (previous is not null)
            {
                lastDiff = FirstDifference(previous, frame);
                if (lastDiff is null) return frames;
            }

            if (frames >= SettleMaxFrames || watch.Elapsed > SettleBudget)
                throw new InvalidOperationException(
                    $"The window never settled: {frames} frame(s) in {watch.ElapsedMilliseconds} ms, still changing at {Describe(window, lastDiff)}. "
                    + "A control that animates forever (a spinner, a caret) must be stopped by the test before it is audited.");
            previous = frame;
            var next = watch.Elapsed + SettleSpacing;
            while (watch.Elapsed < next) Pump(1);
        }
    }

    private static Rect? FirstDifference(PixelBuffer one, PixelBuffer two)
    {
        if (one.Width != two.Width || one.Height != two.Height) return new Rect(0, 0, two.Width, two.Height);
        int minX = int.MaxValue, minY = int.MaxValue, maxX = -1, maxY = -1;
        for (var y = 0; y < one.Height; y++)
        for (var x = 0; x < one.Width; x++)
        {
            if (one.At(x, y) == two.At(x, y)) continue;
            minX = Math.Min(minX, x);
            minY = Math.Min(minY, y);
            maxX = Math.Max(maxX, x);
            maxY = Math.Max(maxY, y);
        }

        return maxX < 0 ? null : new Rect(minX, minY, maxX - minX + 1, maxY - minY + 1);
    }

    private static string Describe(Window window, Rect? rect)
    {
        if (rect is not { } area) return "an unknown area";
        var control = A11yTree.Descendants(window)
            .Where(c => c.IsVisible && c.TranslatePoint(default, window) is { } origin
                && new Rect(origin, c.Bounds.Size).Contains(area.TopLeft))
            .LastOrDefault();
        var who = control is null ? "no control" : $"{control.GetType().Name} '{control.Name}'";
        return $"rectangle {area} ({who})";
    }

    /// <summary>Pump the dispatcher and one render tick, as ScreenshotAssert does.</summary>
    public static void Pump(int ticks = 2)
    {
        for (var tick = 0; tick < ticks; tick++)
        {
            Dispatcher.UIThread.RunJobs();
            Avalonia.Headless.AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
        }
    }

    /// <summary>Close a host window without leaving its content in the tree.</summary>
    public static void Close(Window window)
    {
        ArgumentNullException.ThrowIfNull(window);
        window.Content = null;
        window.Close();
        Pump(1);
    }
}
