using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.TextFormatting.Unicode;
using Avalonia.Platform;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The font bundle contract: both faces and the OFL text really are in the assembly, they are
/// byte-identical to the licensed copies in the lab, the notices file claims what ships, and
/// the token the controls read resolves to the bundled face rather than a host font.
/// </summary>
public sealed class FontResourcesTests
{
    [AvaloniaFact]
    public void BothFacesAndTheLicenceAreCompiledIntoTheAssembly()
    {
        var regular = Read(SpotledgerFonts.UrduRegularAsset);
        var bold = Read(SpotledgerFonts.UrduBoldAsset);
        var licence = System.Text.Encoding.UTF8.GetString(Read(SpotledgerFonts.UrduLicenceAsset));

        Assert.True(regular.Length > 100_000, $"The regular face is only {regular.Length} bytes.");
        Assert.True(bold.Length > 100_000, $"The bold face is only {bold.Length} bytes.");
        Assert.NotEqual(regular, bold);
        Assert.Contains("SIL OPEN FONT LICENSE", licence, StringComparison.Ordinal);
        Assert.Contains("Version 1.1", licence, StringComparison.Ordinal);
        Assert.Contains("Noto Project Authors", licence, StringComparison.Ordinal);

        var latin = Read(SpotledgerFonts.LatinRegularAsset);
        var latinLicence = System.Text.Encoding.UTF8.GetString(Read(SpotledgerFonts.LatinLicenceAsset));

        Assert.True(latin.Length > 400_000, $"The Latin face is only {latin.Length} bytes.");
        Assert.Contains("SIL OPEN FONT LICENSE", latinLicence, StringComparison.Ordinal);
        Assert.Contains("latin-greek-cyrillic", latinLicence, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void TheBundledFacesAreByteIdenticalToTheLicensedLabCopies()
    {
        var lab = Path.Combine(TestResources.RepositoryRoot, "lab", "SPK-03", "assets");

        // The licensed lab copies live in the private repository; the public tree ships only the
        // bundled faces, so there is nothing to compare against there.
        if (!Directory.Exists(lab))
        {
            return;
        }

        Assert.Equal(File.ReadAllBytes(Path.Combine(lab, "NotoNaskhArabic-Regular.ttf")), Read(SpotledgerFonts.UrduRegularAsset));
        Assert.Equal(File.ReadAllBytes(Path.Combine(lab, "NotoNaskhArabic-Bold.ttf")), Read(SpotledgerFonts.UrduBoldAsset));
        Assert.Equal(File.ReadAllBytes(Path.Combine(lab, "NotoNaskhArabic-OFL.txt")), Read(SpotledgerFonts.UrduLicenceAsset));
        Assert.Equal(File.ReadAllBytes(Path.Combine(lab, "NotoSans-Regular.ttf")), Read(SpotledgerFonts.LatinRegularAsset));
        Assert.Equal(File.ReadAllBytes(Path.Combine(lab, "NotoSans-OFL.txt")), Read(SpotledgerFonts.LatinLicenceAsset));
    }

    [Fact]
    public void TheNoticesFileNamesTheFontTheLicenceAndTheSize()
    {
        var notices = File.ReadAllText(Path.Combine(TestResources.RepositoryRoot, "clients", "avalonia", "src", "Spotledger.Client.Theme", "THIRD-PARTY-NOTICES.md"));

        Assert.Contains("Noto Naskh Arabic", notices, StringComparison.Ordinal);
        Assert.Contains("Noto Sans", notices, StringComparison.Ordinal);
        Assert.Contains("SIL Open Font License", notices, StringComparison.Ordinal);
        Assert.Contains("Copyright 2022 The Noto Project Authors", notices, StringComparison.Ordinal);
        Assert.Contains("163,400", notices, StringComparison.Ordinal);
        Assert.Contains("163,504", notices, StringComparison.Ordinal);
        Assert.Contains("453,428", notices, StringComparison.Ordinal);
        Assert.Contains("789,110", notices, StringComparison.Ordinal);
        Assert.Contains("Modifications:** none", notices, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void RegisteringTwiceIsHarmless()
    {
        SpotledgerFonts.Register();
        SpotledgerFonts.Register();

        Assert.True(SpotledgerFonts.IsRegistered);
    }

    [AvaloniaFact]
    public void TheUrduTokenResolvesToTheBundledFaceNotAHostFont()
    {
        SpotledgerFonts.Register();
        var tokenFamily = new FontFamily("fonts:SpotledgerNaskh#Noto Naskh Arabic, Noto Naskh Arabic, Noto Nastaliq Urdu, serif");

        Assert.True(FontManager.Current.TryGetGlyphTypeface(new Typeface(tokenFamily), out var glyphTypeface),
            "The Urdu family token does not resolve to any face.");
        Assert.Equal(SpotledgerFonts.UrduFamilyName, glyphTypeface.FamilyName);
        Assert.True(glyphTypeface.CanShapeScript(Script.Arabic));
    }

    /// <summary>
    /// The bundled family must be reachable from a Latin-family control too: that is how a mixed
    /// field renders Urdu runs without switching the whole control to the Urdu family.
    /// </summary>
    [AvaloniaFact]
    public void ACharacterMatchInsideTheUrduFamilyFindsTheBundledFace()
    {
        SpotledgerFonts.Register();

        Assert.True(FontManager.Current.TryMatchCharacter(
            'ب', FontStyle.Normal, FontWeight.Normal, FontStretch.Normal,
            SpotledgerFonts.Urdu, TestResources.UrduCulture, out var matched),
            "No typeface matched an Urdu letter inside the bundled family.");
        Assert.True(FontManager.Current.TryGetGlyphTypeface(matched, out var glyphTypeface));
        Assert.Equal(SpotledgerFonts.UrduFamilyName, glyphTypeface.FamilyName);
    }

    private static byte[] Read(Uri asset)
    {
        using var stream = AssetLoader.Open(asset);
        using var buffer = new MemoryStream();
        stream.CopyTo(buffer);
        return buffer.ToArray();
    }
}
