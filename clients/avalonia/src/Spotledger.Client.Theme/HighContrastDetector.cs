namespace Spotledger.Client.Theme;

/// <summary>
/// What the platform says about high contrast. <see cref="Dark"/> selects the palette that
/// paints light text on a dark background; it is reported separately because Windows runs
/// both a "High Contrast Black" and a "High Contrast White" theme.
/// </summary>
/// <param name="Enabled">The platform accessibility setting is on.</param>
/// <param name="Dark">The platform's own high-contrast palette is dark.</param>
public readonly record struct HighContrastState(bool Enabled, bool Dark)
{
    /// <summary>High contrast is off: the requested theme variant stands.</summary>
    public static HighContrastState Disabled { get; }

    /// <summary>The token variant this state calls for, ignoring the user's own choice.</summary>
    public ThemeVariantKind Variant => Enabled
        ? (Dark ? ThemeVariantKind.HighContrastDark : ThemeVariantKind.HighContrast)
        : (Dark ? ThemeVariantKind.Dark : ThemeVariantKind.Light);
}

/// <summary>
/// The one platform question the theme asks: is high contrast on, and is its palette dark?
/// Implementations are pure reads with no side effects, so a test can substitute one and a
/// host can poll one per frame.
/// </summary>
public interface IHighContrastDetector
{
    /// <summary>Reads the current platform state.</summary>
    HighContrastState Read();
}

/// <summary>
/// The platform-neutral implementation: always "off". Every OS that does not expose a
/// high-contrast switch (Linux, macOS, the browser) uses it, and so does a Windows host
/// that has not yet been wired to <see cref="WindowsHighContrastDetector"/>.
/// </summary>
public sealed class NoHighContrastDetector : IHighContrastDetector
{
    /// <summary>The shared stateless instance.</summary>
    public static NoHighContrastDetector Instance { get; } = new();

    /// <inheritdoc />
    public HighContrastState Read() => HighContrastState.Disabled;
}

/// <summary>
/// Creates the detector for the running platform and turns its answer into a token variant.
/// The Windows implementation is the only one that reads a real setting; everywhere else
/// this returns <see cref="NoHighContrastDetector"/>.
/// </summary>
public static class HighContrastDetector
{
    /// <summary>The <c>HCF_HIGHCONTRASTON</c> bit of the Windows <c>HIGHCONTRAST.dwFlags</c>.</summary>
    public const uint HighContrastOnFlag = 0x00000001;

    /// <summary>
    /// The detector for this process. The Windows P/Invoke is only reachable when
    /// <see cref="OperatingSystem.IsWindows()"/> is true; on every other platform the
    /// no-op detector is returned and no native code runs.
    /// </summary>
    public static IHighContrastDetector Create() =>
        OperatingSystem.IsWindows() ? new WindowsHighContrastDetector() : NoHighContrastDetector.Instance;

    /// <summary>
    /// Decodes the Windows <c>HIGHCONTRAST.dwFlags</c> value. Pure, so the flag handling is
    /// testable on any OS. High contrast never implies a dark palette here: only the caller's
    /// own theme choice supplies that, which is why the Windows detector reports
    /// <c>Dark: false</c> and a dark high-contrast palette stays a user setting.
    /// </summary>
    public static HighContrastState FromHighContrastFlags(uint flags) =>
        new((flags & HighContrastOnFlag) != 0, Dark: false);

    /// <summary>
    /// The variant to apply: high contrast overrides the user's Light/Dark choice, and
    /// leaves it alone when the platform reports high contrast off.
    /// </summary>
    public static ThemeVariantKind Resolve(ThemeVariantKind requested, HighContrastState state) =>
        state.Enabled ? state.Variant : requested;
}
