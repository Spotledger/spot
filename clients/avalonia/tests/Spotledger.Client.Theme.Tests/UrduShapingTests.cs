using System.Text;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.TextFormatting;
using Avalonia.Media.TextFormatting.Unicode;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The no-tofu check. Every string in the Urdu fixture is shaped with the bundled Noto Naskh
/// Arabic face — HarfBuzz through the Skia text shaper the application really uses — and any
/// glyph that comes back as <c>.notdef</c> (glyph index 0) fails the test, naming the string id
/// and the code point that could not be shaped. A character the face has no entry for is
/// reported the same way, because a missing cmap entry and a missing shape are the same box to
/// the user.
///
/// The bundled face is used explicitly (never the ambient font manager pick), so a system
/// Arabic font on the machine running the test cannot hide a gap in the bundle.
/// </summary>
public sealed class UrduShapingTests
{
    private const double ShapingSize = 16;

    [AvaloniaFact]
    public void BundledUrduFaceIsRegisteredAndShapesArabic()
    {
        SpotledgerFonts.Register();

        var glyphTypeface = SpotledgerFonts.ResolveUrduGlyphTypeface();

        Assert.NotNull(glyphTypeface);
        Assert.Equal(SpotledgerFonts.UrduFamilyName, glyphTypeface.FamilyName);
        Assert.True(glyphTypeface.CanShapeScript(Script.Arabic), "The bundled face claims it cannot shape Arabic script.");
        Assert.True(glyphTypeface.SupportsScript(Script.Arabic), "The bundled face does not declare Arabic script support.");
    }

    [Fact]
    public void UrduResourceSetHasAtLeastThreeHundredStringsWithUniqueIds()
    {
        var strings = TestResources.Urdu;

        Assert.True(strings.Count >= 300, $"The Urdu resource set has only {strings.Count} strings; at least 300 are required.");
        Assert.Equal(strings.Count, strings.Select(entry => entry.Id).Distinct(StringComparer.Ordinal).Count());
        Assert.All(strings, entry => Assert.False(string.IsNullOrWhiteSpace(entry.Text), $"{entry.Id} has no text."));
        Assert.All(strings, entry => Assert.True(BidiText.HasArabicScript(entry.Text), $"{entry.Id} holds no Arabic-script text."));
        // The fixture must not accidentally rely on a Latin glyph the bundled face lacks.
        Assert.DoesNotContain(strings, entry => entry.Text.Any(character => character is >= 'A' and <= 'z'));
    }

    [AvaloniaFact]
    public void EveryUrduStringShapesWithoutAMissingGlyph()
    {
        var glyphTypeface = SpotledgerFonts.ResolveUrduGlyphTypeface();
        Assert.NotNull(glyphTypeface);
        var options = Options(glyphTypeface);
        var failures = new List<string>();
        var shaped = 0;

        foreach (var entry in TestResources.Urdu)
        {
            failures.AddRange(MissingGlyphs(entry, options));
            shaped++;
        }

        Assert.True(failures.Count == 0,
            $"{failures.Count} missing glyph(s) across {shaped} strings:{Environment.NewLine}{string.Join(Environment.NewLine, failures)}");
        Assert.True(shaped >= 300, $"Only {shaped} strings were shaped.");
    }

    /// <summary>
    /// The documented boundary, asserted rather than assumed: the bundled Naskh face has no
    /// Latin coverage (it is an Arabic-script font), so a mixed field must not be rendered with
    /// the Urdu family alone. Every uncovered code point in the mixed fixture is ASCII — a Latin
    /// letter, a digit-adjacent symbol or punctuation — which the fallback family supplies; no
    /// Arabic-script character is ever uncovered.
    /// </summary>
    [AvaloniaFact]
    public void MixedFieldsOnlyLackLatinCoverageWhichTheFallbackFamilySupplies()
    {
        var glyphTypeface = SpotledgerFonts.ResolveUrduGlyphTypeface();
        Assert.NotNull(glyphTypeface);
        var options = Options(glyphTypeface);
        var uncovered = new SortedSet<int>();

        foreach (var entry in TestResources.Mixed)
        {
            foreach (var rune in entry.Text.EnumerateRunes())
            {
                if (!glyphTypeface.CharacterToGlyphMap.ContainsGlyph(rune.Value))
                {
                    uncovered.Add(rune.Value);
                }
            }
        }

        foreach (var codePoint in uncovered)
        {
            Assert.True(codePoint < 0x80,
                $"U+{codePoint:X4} ({char.ConvertFromUtf32(codePoint)}) is Arabic-script and has no glyph in the bundled face.");
        }

        Assert.NotEmpty(uncovered);
        // The Urdu runs of every mixed field still shape cleanly with the bundled face.
        var urduRuns = TestResources.Mixed.Select(entry => entry with { Text = UrduOnly(entry.Text) }).ToArray();
        var failures = urduRuns.SelectMany(entry => MissingGlyphs(entry, options)).ToArray();
        Assert.True(failures.Length == 0, string.Join(Environment.NewLine, failures));
    }

    [AvaloniaFact]
    public void ShapingTheSameStringTwiceGivesTheSameGlyphs()
    {
        var glyphTypeface = SpotledgerFonts.ResolveUrduGlyphTypeface();
        Assert.NotNull(glyphTypeface);
        var options = Options(glyphTypeface);
        const string Sample = "رسید نمبر اور تاریخ";

        var first = GlyphIndices(Sample, options);
        var second = GlyphIndices(Sample, options);

        Assert.Equal(first, second);
        Assert.DoesNotContain((ushort)0, first);
    }

    [AvaloniaFact]
    public void TypedEasternDigitsShapeWithTheBundledFace()
    {
        var glyphTypeface = SpotledgerFonts.ResolveUrduGlyphTypeface();
        Assert.NotNull(glyphTypeface);
        var options = Options(glyphTypeface);

        foreach (var text in new[] { "١٢٣٫٤٥", "۱۲۳٫۴۵", "۱۲ اکتوبر ۲۰۲۶" })
        {
            Assert.All(text.EnumerateRunes(), rune =>
                Assert.True(glyphTypeface.CharacterToGlyphMap.ContainsGlyph(rune.Value),
                    $"U+{rune.Value:X4} in '{text}' has no glyph in the bundled face."));
            Assert.DoesNotContain((ushort)0, GlyphIndices(text, options));
        }
    }

    private static TextShaperOptions Options(GlyphTypeface glyphTypeface) =>
        new(glyphTypeface, ShapingSize, bidiLevel: 1, TestResources.UrduCulture,
            incrementalTabWidth: 0, letterSpacing: 0, fontFeatures: []);

    private static IEnumerable<string> MissingGlyphs(UrduString entry, TextShaperOptions options)
    {
        using var buffer = TextShaper.Current.ShapeText(entry.Text, options);
        for (var index = 0; index < buffer.Length; index++)
        {
            var glyph = buffer[index];
            if (glyph.GlyphIndex != 0)
            {
                continue;
            }

            var cluster = glyph.GlyphCluster >= 0 && glyph.GlyphCluster < entry.Text.Length
                ? glyph.GlyphCluster
                : 0;
            var codePoint = char.ConvertToUtf32(entry.Text, cluster);
            yield return $"{entry.Id} ({entry.Category}): missing glyph for U+{codePoint:X4} '{(char.IsSurrogatePair(entry.Text, cluster) ? entry.Text.Substring(cluster, 2) : entry.Text[cluster].ToString())}' in \"{entry.Text}\".";
            yield break;
        }
    }

    private static ushort[] GlyphIndices(string text, TextShaperOptions options)
    {
        using var buffer = TextShaper.Current.ShapeText(text, options);
        var glyphs = new ushort[buffer.Length];
        for (var index = 0; index < buffer.Length; index++)
        {
            glyphs[index] = buffer[index].GlyphIndex;
        }

        return glyphs;
    }

    /// <summary>The Arabic-script characters of a mixed string, in order.</summary>
    private static string UrduOnly(string text)
    {
        var builder = new StringBuilder(text.Length);
        foreach (var rune in text.EnumerateRunes())
        {
            if (rune.Value is >= 0x0600 and <= 0x06FF
                or >= 0x0750 and <= 0x077F
                or >= 0x08A0 and <= 0x08FF
                or >= 0xFB50 and <= 0xFDFF
                or >= 0xFE70 and <= 0xFEFF
                or ' ')
            {
                builder.Append(rune.ToString());
            }
        }

        return builder.ToString();
    }
}
