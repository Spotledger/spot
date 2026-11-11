using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The high-contrast detector: the platform question is a pure read, the Windows flag decode is
/// testable on any OS, and the no-op detector is what every other platform gets.
/// </summary>
public sealed class HighContrastTests
{
    [Fact]
    public void TheFlagDecodeReadsOnlyTheHighContrastBit()
    {
        Assert.Equal(HighContrastState.Disabled, HighContrastDetector.FromHighContrastFlags(0));
        Assert.True(HighContrastDetector.FromHighContrastFlags(HighContrastDetector.HighContrastOnFlag).Enabled);

        // HCF_HIGHCONTRASTON | HCF_CONFIRMHOTKEY: the other bits must not change the answer.
        Assert.True(HighContrastDetector.FromHighContrastFlags(0x00000001 | 0x00000020).Enabled);
        Assert.False(HighContrastDetector.FromHighContrastFlags(0x00000020).Enabled);

        // The flag alone never claims a dark palette: only the user's own choice does that.
        Assert.False(HighContrastDetector.FromHighContrastFlags(HighContrastDetector.HighContrastOnFlag).Dark);
    }

    [Fact]
    public void HighContrastOverridesTheRequestedVariantAndOtherwiseLeavesIt()
    {
        Assert.Equal(ThemeVariantKind.Light, HighContrastDetector.Resolve(ThemeVariantKind.Light, HighContrastState.Disabled));
        Assert.Equal(ThemeVariantKind.Dark, HighContrastDetector.Resolve(ThemeVariantKind.Dark, HighContrastState.Disabled));
        Assert.Equal(ThemeVariantKind.HighContrast, HighContrastDetector.Resolve(ThemeVariantKind.Dark, new HighContrastState(true, false)));
        Assert.Equal(ThemeVariantKind.HighContrastDark, HighContrastDetector.Resolve(ThemeVariantKind.Dark, new HighContrastState(true, true)));
    }

    [Fact]
    public void TheStateMapsOntoTheFourPalettes()
    {
        Assert.Equal(ThemeVariantKind.Light, HighContrastState.Disabled.Variant);
        Assert.Equal(ThemeVariantKind.Dark, new HighContrastState(false, true).Variant);
        Assert.Equal(ThemeVariantKind.HighContrast, new HighContrastState(true, false).Variant);
        Assert.Equal(ThemeVariantKind.HighContrastDark, new HighContrastState(true, true).Variant);
    }

    [Fact]
    public void TheCreatedDetectorIsThePlatformsAndNeverThrows()
    {
        var detector = HighContrastDetector.Create();

        var state = detector.Read();

        // On Linux/macOS the no-op detector answers; on Windows the P/Invoke may fail in a
        // locked-down session, which must also answer "off" rather than throw.
        Assert.False(state.Enabled);
        Assert.False(state.Dark);
        if (!OperatingSystem.IsWindows())
        {
            Assert.Same(NoHighContrastDetector.Instance, detector);
        }
    }

    [Fact]
    public void TheNoOpDetectorIsStatelessAndOff()
    {
        Assert.Equal(HighContrastState.Disabled, NoHighContrastDetector.Instance.Read());
        Assert.Same(NoHighContrastDetector.Instance, NoHighContrastDetector.Instance);
    }

    [Fact]
    public void TheVariantEnumCoversExactlyTheFourPalettes()
    {
        Assert.Equal(4, Enum.GetValues<ThemeVariantKind>().Length);
        Assert.All(Enum.GetValues<ThemeVariantKind>(), kind => Assert.Equal(kind, ThemeVariants.FromThemeVariant(kind.ToThemeVariant())));
        Assert.True(ThemeVariantKind.HighContrast.IsHighContrast());
        Assert.True(ThemeVariantKind.HighContrastDark.IsHighContrast());
        Assert.True(ThemeVariantKind.HighContrastDark.IsDark());
        Assert.False(ThemeVariantKind.HighContrast.IsDark());
    }
}
