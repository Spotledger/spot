using System.Globalization;
using System.Text.Json;
using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Spotledger.Client.Theme;
using Spotledger.TokenGenerator;
using Xunit;

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The token contract: every colour exists in every variant, the accessibility palettes meet
/// their contrast floors, the generated files match the source, and applying a variant really
/// resolves that variant's values. A hand edit of a generated file, a missing variant value or
/// a low-contrast pair fails here rather than in a review.
/// </summary>
public sealed class TokenContractTests
{
    private const double AaaTextContrast = 7.0;
    private const double AaTextContrast = 4.5;
    private const double NonTextContrast = 3.0;

    private static readonly string[] OrdinalVariants = ["light", "dark", "highContrast", "highContrastDark"];

    [Fact]
    public void EveryTokenHasAValueAndADeclaredOrInheritedDtcgType()
    {
        using var document = LoadTokens();
        var tokenCount = 0;

        void Visit(JsonElement group, string? inheritedType)
        {
            var type = group.TryGetProperty("$type", out var typeNode) ? typeNode.GetString() : inheritedType;
            if (group.TryGetProperty("$value", out var value))
            {
                Assert.False(string.IsNullOrWhiteSpace(type));
                Assert.NotEqual(JsonValueKind.Null, value.ValueKind);
                tokenCount++;
                return;
            }

            foreach (var child in group.EnumerateObject().Where(property => property.Name[0] != '$'))
            {
                Visit(child.Value, type);
            }
        }

        foreach (var group in document.RootElement.EnumerateObject().Where(property => property.Name[0] != '$'))
        {
            Visit(group.Value, inheritedType: null);
        }

        Assert.True(tokenCount > 0);
    }

    [Fact]
    public void EveryColorTokenExistsInEveryVariant()
    {
        using var document = LoadTokens();
        var color = document.RootElement.GetProperty("color");
        var declared = color.EnumerateObject().Where(property => property.Name[0] != '$').Select(property => property.Name).ToArray();
        Assert.Equal(OrdinalVariants.Order(StringComparer.Ordinal), declared.Order(StringComparer.Ordinal));

        var palettes = declared.ToDictionary(
            variant => variant,
            variant => CollectColorValues(color.GetProperty(variant)),
            StringComparer.Ordinal);

        var reference = palettes["light"].Keys.Order(StringComparer.Ordinal).ToArray();
        Assert.NotEmpty(reference);
        foreach (var variant in declared)
        {
            Assert.Equal(reference, palettes[variant].Keys.Order(StringComparer.Ordinal));
            Assert.All(palettes[variant].Values, value => Assert.Matches("^#[0-9A-F]{6}$", value));
        }
    }

    [Fact]
    public void OrdinaryTextMeetsAaAndBothHighContrastPalettesMeetAaa()
    {
        using var document = LoadTokens();
        var color = document.RootElement.GetProperty("color");
        var failures = new List<string>();

        foreach (var variant in OrdinalVariants)
        {
            var palette = color.GetProperty(variant);
            var surfaces = palette.GetProperty("surface");
            var highContrast = variant is "highContrast" or "highContrastDark";
            var required = highContrast ? AaaTextContrast : AaTextContrast;
            var foregrounds = new List<(string Name, string Value)>
            {
                ("text.primary", Text(palette, "text", "primary")),
                ("text.secondary", Text(palette, "text", "secondary")),
            };
            if (highContrast)
            {
                foregrounds.Add(("text.disabled", Text(palette, "text", "disabled")));
                foreach (var semantic in palette.GetProperty("semantic").EnumerateObject())
                {
                    foregrounds.Add(($"semantic.{semantic.Name}", semantic.Value.GetProperty("$value").GetString()!));
                }
            }

            foreach (var (name, foreground) in foregrounds)
            {
                foreach (var surface in surfaces.EnumerateObject())
                {
                    var ratio = ContrastRatio(foreground, surface.Value.GetProperty("$value").GetString()!);
                    if (ratio < required)
                    {
                        failures.Add($"{variant} {name} on surface.{surface.Name} is {ratio.ToString("0.00", CultureInfo.InvariantCulture)}:1, needs {required:0.0}:1.");
                    }
                }
            }
        }

        Assert.True(failures.Count == 0, string.Join(Environment.NewLine, failures));
    }

    [Fact]
    public void BordersAndTheFocusRingAreVisibleInEveryVariant()
    {
        using var document = LoadTokens();
        var color = document.RootElement.GetProperty("color");
        var failures = new List<string>();

        foreach (var variant in OrdinalVariants)
        {
            var palette = color.GetProperty(variant);
            var surfaces = palette.GetProperty("surface");
            var ring = Text(palette, "focus", "ring");
            foreach (var surface in surfaces.EnumerateObject())
            {
                var ratio = ContrastRatio(ring, surface.Value.GetProperty("$value").GetString()!);
                if (ratio < NonTextContrast)
                {
                    failures.Add($"{variant} focus.ring on surface.{surface.Name} is {ratio.ToString("0.00", CultureInfo.InvariantCulture)}:1, needs {NonTextContrast:0.0}:1.");
                }
            }

            if (variant is not ("highContrast" or "highContrastDark"))
            {
                continue;
            }

            foreach (var border in new[] { "default", "strong" })
            {
                foreach (var surface in new[] { "canvas", "raised" })
                {
                    var ratio = ContrastRatio(Text(palette, "border", border), Text(palette, "surface", surface));
                    if (ratio < NonTextContrast)
                    {
                        failures.Add($"{variant} border.{border} on surface.{surface} is {ratio.ToString("0.00", CultureInfo.InvariantCulture)}:1, needs {NonTextContrast:0.0}:1.");
                    }
                }
            }
        }

        Assert.True(failures.Count == 0, string.Join(Environment.NewLine, failures));
    }

    [Fact]
    public void TheFocusRingIsAtLeastThreePixelsWithARealGap()
    {
        using var document = LoadTokens();
        var focus = document.RootElement.GetProperty("focus");

        Assert.Equal("dimension", focus.GetProperty("$type").GetString());
        Assert.True(Pixels(focus.GetProperty("width")) >= 3, "The focus ring must be at least 3 px.");
        Assert.True(Pixels(focus.GetProperty("offset")) >= 0);
    }

    [Fact]
    public void GeneratedAvaloniaResourcesAndTypedKeysMatchTheSource()
    {
        var tokens = File.ReadAllText(TokensPath());
        var generated = global::Spotledger.TokenGenerator.TokenGenerator.Generate(tokens);
        var themeDirectory = ThemeDirectory();

        Assert.Equal(generated.Axaml.ReplaceLineEndings(), File.ReadAllText(Path.Combine(themeDirectory, "Tokens.axaml")).ReplaceLineEndings());
        Assert.Equal(generated.CSharp.ReplaceLineEndings(), File.ReadAllText(Path.Combine(themeDirectory, "Tokens.cs")).ReplaceLineEndings());
    }

    [Fact]
    public void TheGeneratedFileDeclaresOneThemeDictionaryPerVariant()
    {
        var axaml = File.ReadAllText(Path.Combine(ThemeDirectory(), "Tokens.axaml"));

        foreach (var variant in OrdinalVariants)
        {
            var expected = variant switch
            {
                "light" => "x:Key=\"Light\"",
                "dark" => "x:Key=\"Dark\"",
                _ => $"x:Key=\"{{x:Static theme:ThemeVariants.{char.ToUpperInvariant(variant[0])}{variant[1..]}}}\"",
            };
            Assert.Contains(expected, axaml, StringComparison.Ordinal);
        }

        Assert.Contains("Color.Focus.Ring", axaml, StringComparison.Ordinal);
        Assert.Contains("Focus.Width", axaml, StringComparison.Ordinal);
    }

    [Fact]
    public void AHandEditedTokenFileIsDetectedAsStale()
    {
        var tokens = File.ReadAllText(TokensPath());
        var committed = global::Spotledger.TokenGenerator.TokenGenerator.Generate(tokens);
        var edited = tokens.Replace("#CBD5E1", "#CBD5E2", StringComparison.Ordinal);
        Assert.NotEqual(tokens, edited);

        var regenerated = global::Spotledger.TokenGenerator.TokenGenerator.Generate(edited);

        Assert.NotEqual(committed.Axaml, regenerated.Axaml);
    }

    [Fact]
    public void MissingVariantValueIsRejectedByTheGenerator()
    {
        using var document = JsonDocument.Parse(File.ReadAllText(TokensPath()));
        var color = document.RootElement.GetProperty("color");
        var highContrast = color.GetProperty("highContrast");
        var broken = new Dictionary<string, object?>(StringComparer.Ordinal)
        {
            ["color"] = new Dictionary<string, object?>(StringComparer.Ordinal)
            {
                ["light"] = JsonSerializer.Deserialize<object>(color.GetProperty("light").GetRawText()),
                ["dark"] = JsonSerializer.Deserialize<object>(color.GetProperty("dark").GetRawText()),
                // The high-contrast palette loses its focus ring: every variant must carry it.
                ["highContrast"] = JsonSerializer.Deserialize<object>(
                    JsonSerializer.Serialize(WithoutFocusRing(highContrast))),
                ["highContrastDark"] = JsonSerializer.Deserialize<object>(color.GetProperty("highContrastDark").GetRawText()),
            },
        };

        var exception = Assert.Throws<InvalidOperationException>(
            () => global::Spotledger.TokenGenerator.TokenGenerator.Generate(JsonSerializer.Serialize(broken)));
        Assert.Contains("Color.Focus.Ring", exception.Message, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void ApplyingEachVariantResolvesThatVariantsOwnValues()
    {
        var application = Application.Current!;
        using var document = LoadTokens();

        foreach (var kind in Enum.GetValues<ThemeVariantKind>())
        {
            SpotledgerTheme.Apply(application, kind);

            Assert.Equal(kind, SpotledgerTheme.ActiveVariant(application));
            Assert.True(SpotledgerTheme.TryGetToken(application, Tokens.TokenColorTextPrimary, kind, out var value));
            var primary = Assert.IsType<Color>(value);
            Assert.Equal(Hex(primary), Text(document.RootElement.GetProperty("color").GetProperty(VariantName(kind)), "text", "primary"));

            Assert.True(SpotledgerTheme.TryGetToken(application, Tokens.TokenColorSurfaceCanvas, kind, out var canvas));
            Assert.Equal(Hex(Assert.IsType<Color>(canvas)),
                Text(document.RootElement.GetProperty("color").GetProperty(VariantName(kind)), "surface", "canvas"));
        }

        SpotledgerTheme.Apply(application, ThemeVariantKind.Light);
    }

    [AvaloniaFact]
    public void HighContrastWinsOverTheUsersLightOrDarkChoice()
    {
        var application = Application.Current!;
        SpotledgerTheme.Apply(application, ThemeVariantKind.Dark);

        var applied = SpotledgerTheme.ApplyForContrast(application, new FixedDetector(new HighContrastState(true, false)));

        Assert.Equal(ThemeVariantKind.HighContrast, applied);
        Assert.Equal(ThemeVariantKind.HighContrast, SpotledgerTheme.ActiveVariant(application));

        SpotledgerTheme.Apply(application, ThemeVariantKind.Light);
        Assert.Equal(ThemeVariantKind.Light, SpotledgerTheme.ActiveVariant(application));
    }

    private sealed class FixedDetector(HighContrastState state) : IHighContrastDetector
    {
        public HighContrastState Read() => state;
    }

    private static JsonElement WithoutFocusRing(JsonElement palette)
    {
        var copy = JsonSerializer.Deserialize<Dictionary<string, JsonElement>>(palette.GetRawText())!;
        copy.Remove("focus");
        using var document = JsonDocument.Parse(JsonSerializer.Serialize(copy));
        return document.RootElement.Clone();
    }

    private static string VariantName(ThemeVariantKind kind) => kind switch
    {
        ThemeVariantKind.Light => "light",
        ThemeVariantKind.Dark => "dark",
        ThemeVariantKind.HighContrast => "highContrast",
        ThemeVariantKind.HighContrastDark => "highContrastDark",
        _ => throw new ArgumentOutOfRangeException(nameof(kind), kind, "Unknown variant."),
    };

    private static string Hex(Color color) => $"#{color.R:X2}{color.G:X2}{color.B:X2}";

    private static string Text(JsonElement palette, string group, string name) =>
        palette.GetProperty(group).GetProperty(name).GetProperty("$value").GetString()!;

    private static double Pixels(JsonElement token)
    {
        var value = token.GetProperty("$value").GetString()!;
        Assert.EndsWith("px", value, StringComparison.Ordinal);
        return double.Parse(value[..^2], NumberStyles.Float, CultureInfo.InvariantCulture);
    }

    private static JsonDocument LoadTokens() => JsonDocument.Parse(File.ReadAllText(TokensPath()));

    private static string TokensPath() => Path.Combine(FindRepositoryRoot(), "clients", "avalonia", "design", "tokens.json");

    private static string ThemeDirectory() => Path.Combine(FindRepositoryRoot(), "clients", "avalonia", "src", "Spotledger.Client.Theme");

    private static Dictionary<string, string> CollectColorValues(JsonElement group)
    {
        var values = new Dictionary<string, string>(StringComparer.Ordinal);

        void Visit(JsonElement node, string path)
        {
            if (node.TryGetProperty("$value", out var value))
            {
                values.Add(path, value.GetString()!);
                return;
            }

            foreach (var property in node.EnumerateObject().Where(property => property.Name[0] != '$'))
            {
                var childPath = path.Length == 0 ? property.Name : $"{path}.{property.Name}";
                Visit(property.Value, childPath);
            }
        }

        Visit(group, string.Empty);
        return values;
    }

    private static double ContrastRatio(string foreground, string background)
    {
        var first = RelativeLuminance(ParseColor(foreground));
        var second = RelativeLuminance(ParseColor(background));
        var lighter = Math.Max(first, second);
        var darker = Math.Min(first, second);
        return (lighter + 0.05) / (darker + 0.05);
    }

    private static (byte Red, byte Green, byte Blue) ParseColor(string value)
    {
        Assert.Matches("^#[0-9A-Fa-f]{6}$", value);
        return (
            byte.Parse(value.AsSpan(1, 2), NumberStyles.HexNumber, CultureInfo.InvariantCulture),
            byte.Parse(value.AsSpan(3, 2), NumberStyles.HexNumber, CultureInfo.InvariantCulture),
            byte.Parse(value.AsSpan(5, 2), NumberStyles.HexNumber, CultureInfo.InvariantCulture));
    }

    private static double RelativeLuminance((byte Red, byte Green, byte Blue) color)
    {
        static double Linear(byte channel)
        {
            var value = channel / 255d;
            return value <= 0.04045 ? value / 12.92 : Math.Pow((value + 0.055) / 1.055, 2.4);
        }

        return 0.2126 * Linear(color.Red) + 0.7152 * Linear(color.Green) + 0.0722 * Linear(color.Blue);
    }

    private static string FindRepositoryRoot()
    {
        for (DirectoryInfo? directory = new(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
        {
            if (File.Exists(Path.Combine(directory.FullName, "clients", "avalonia", "design", "tokens.json")))
            {
                return directory.FullName;
            }
        }

        throw new DirectoryNotFoundException("Could not find the repository root from the test output directory.");
    }
}
