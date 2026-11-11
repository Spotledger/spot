using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Spotledger.TokenGenerator;

public sealed record GeneratedTokenFiles(string Axaml, string CSharp);

public static class TokenGenerator
{
    /// <summary>
    /// The variant order the generator writes theme dictionaries in. Known variants come
    /// first in this order (so the generated file is stable), any additional variant found
    /// in the JSON is appended alphabetically — the generator never silently drops one.
    /// </summary>
    public static IReadOnlyList<string> CanonicalVariants { get; } = ["light", "dark", "highContrast", "highContrastDark"];

    public static GeneratedTokenFiles Generate(string json)
    {
        using var document = JsonDocument.Parse(json);
        var root = document.RootElement;
        var colors = new List<TokenValue>();
        var shared = new List<TokenValue>();

        var colorGroup = root.GetProperty("color");
        var variants = Variants(colorGroup);
        foreach (var mode in variants)
        {
            Walk(colorGroup.GetProperty(mode), ["Color"], null, mode, colors);
        }

        foreach (var property in root.EnumerateObject())
        {
            if (property.Name is "$schema" or "color")
            {
                continue;
            }

            Walk(property.Value, [Pascal(property.Name)], null, null, shared);
        }

        var colorGroups = colors.GroupBy(token => token.ResourceKey, StringComparer.Ordinal).ToArray();
        foreach (var group in colorGroups)
        {
            var themes = group.Select(token => token.Theme!).ToHashSet(StringComparer.Ordinal);
            if (group.Count() != variants.Length || !themes.SetEquals(variants))
            {
                throw new InvalidOperationException(
                    $"Color token '{group.Key}' must define exactly one value for every theme variant ({string.Join(", ", variants)}); found ({string.Join(", ", themes.Order(StringComparer.Ordinal))}).");
            }
        }

        var allTokens = shared.Concat(colors).DistinctBy(token => token.ResourceKey, StringComparer.Ordinal)
            .OrderBy(token => token.ResourceKey, StringComparer.Ordinal).ToArray();
        if (allTokens.Select(token => Identifier(token.ResourceKey)).Distinct(StringComparer.Ordinal).Count() != allTokens.Length)
        {
            throw new InvalidOperationException("Token paths must produce unique C# identifiers.");
        }

        return new GeneratedTokenFiles(NormalizeNewLines(RenderAxaml(shared, colors)), NormalizeNewLines(RenderCSharp(allTokens)));
    }

    private static string NormalizeNewLines(string text) => text.Replace("\r\n", "\n", StringComparison.Ordinal);

    /// <summary>
    /// The theme variants declared under <c>color</c>: every property that is not a DTCG
    /// <c>$</c> annotation. Known names keep the canonical order; unknown ones follow
    /// alphabetically so the generated file never depends on JSON key order.
    /// </summary>
    private static string[] Variants(JsonElement colorGroup)
    {
        var declared = colorGroup.EnumerateObject()
            .Where(property => property.Name.Length > 0 && property.Name[0] != '$')
            .Select(property => property.Name)
            .ToArray();
        if (declared.Length == 0)
        {
            throw new InvalidOperationException("The color group must declare at least one theme variant.");
        }

        var known = CanonicalVariants.Where(name => declared.Contains(name, StringComparer.Ordinal));
        var unknown = declared.Where(name => !CanonicalVariants.Contains(name, StringComparer.Ordinal)).Order(StringComparer.Ordinal);
        var ordered = known.Concat(unknown).ToArray();
        foreach (var required in new[] { "light", "dark" })
        {
            if (!ordered.Contains(required, StringComparer.Ordinal))
            {
                throw new InvalidOperationException($"The color group must declare the '{required}' theme variant.");
            }
        }

        return ordered;
    }

    /// <summary>
    /// The <c>x:Key</c> value of a variant's theme dictionary. Light and Dark are built-in
    /// <see cref="Avalonia.Styling.ThemeVariant"/> keys the XAML type converter understands;
    /// any other variant must name a static <c>ThemeVariant</c> member on the theme's
    /// <c>ThemeVariants</c> class, because the converter refuses custom keys given as text.
    /// </summary>
    private static string ThemeKey(string variant) => variant switch
    {
        "light" => "Light",
        "dark" => "Dark",
        _ => "{x:Static theme:ThemeVariants." + Pascal(variant) + "}",
    };

    /// <summary>Canonical variant order for the generated theme dictionaries.</summary>
    private static int CanonicalOrder(string left, string right)
    {
        var leftIndex = VariantIndex(left);
        var rightIndex = VariantIndex(right);
        return leftIndex != rightIndex ? leftIndex.CompareTo(rightIndex) : string.CompareOrdinal(left, right);

        static int VariantIndex(string name)
        {
            for (var index = 0; index < CanonicalVariants.Count; index++)
            {
                if (string.Equals(CanonicalVariants[index], name, StringComparison.Ordinal))
                {
                    return index;
                }
            }

            return CanonicalVariants.Count;
        }
    }

    private static void Walk(JsonElement node, List<string> path, string? inheritedType, string? theme, List<TokenValue> output)
    {
        if (node.ValueKind != JsonValueKind.Object)
        {
            throw new InvalidOperationException($"Token group '{string.Join('.', path)}' must be an object.");
        }

        var type = node.TryGetProperty("$type", out var typeElement) ? typeElement.GetString() : inheritedType;
        if (node.TryGetProperty("$value", out var value))
        {
            if (string.IsNullOrWhiteSpace(type))
            {
                throw new InvalidOperationException($"Token '{string.Join('.', path)}' must declare or inherit a $type.");
            }

            output.Add(new TokenValue(string.Join('.', path), type, ReadValue(value), theme));
            return;
        }

        var hasChildren = false;
        foreach (var property in node.EnumerateObject())
        {
            if (property.Name[0] == '$')
            {
                continue;
            }

            hasChildren = true;
            path.Add(Pascal(property.Name));
            Walk(property.Value, path, type, theme, output);
            path.RemoveAt(path.Count - 1);
        }

        if (!hasChildren)
        {
            throw new InvalidOperationException($"Group '{string.Join('.', path)}' has no tokens.");
        }
    }

    private static string ReadValue(JsonElement value) => value.ValueKind switch
    {
        JsonValueKind.String => value.GetString() ?? string.Empty,
        JsonValueKind.Number => value.GetRawText(),
        JsonValueKind.True => "true",
        JsonValueKind.False => "false",
        _ => throw new InvalidOperationException("Composite token values are not supported by this generator.")
    };

    private static string RenderAxaml(IReadOnlyCollection<TokenValue> shared, IReadOnlyCollection<TokenValue> colors)
    {
        var output = new StringBuilder();
        output.AppendLine("<ResourceDictionary xmlns=\"https://github.com/avaloniaui\" xmlns:x=\"http://schemas.microsoft.com/winfx/2006/xaml\" xmlns:theme=\"using:Spotledger.Client.Theme\">");
        output.AppendLine("  <ResourceDictionary.ThemeDictionaries>");
        foreach (var mode in colors.Select(token => token.Theme!).Distinct(StringComparer.Ordinal)
                     .OrderBy(mode => mode, Comparer<string>.Create(CanonicalOrder)))
        {
            output.Append("    <ResourceDictionary x:Key=\"").Append(ThemeKey(mode)).AppendLine("\">");
            foreach (var token in colors.Where(token => token.Theme == mode).OrderBy(token => token.ResourceKey, StringComparer.Ordinal))
            {
                RenderEntry(output, token, "      ");
            }

            output.AppendLine("    </ResourceDictionary>");
        }

        output.AppendLine("  </ResourceDictionary.ThemeDictionaries>");
        foreach (var token in shared.OrderBy(token => token.ResourceKey, StringComparer.Ordinal))
        {
            RenderEntry(output, token, "  ");
        }

        output.AppendLine("</ResourceDictionary>");
        return output.ToString();
    }

    private static void RenderEntry(StringBuilder output, TokenValue token, string indentation)
    {
        var (type, value) = RenderXamlValue(token.Type, token.Value);
        output.Append(indentation).Append('<').Append(type).Append(" x:Key=\"").Append(Xml(token.ResourceKey)).Append("\">")
            .Append(Xml(value)).Append("</").Append(type).AppendLine(">");
    }

    private static (string Type, string Value) RenderXamlValue(string type, string value) => type switch
    {
        "color" => ("Color", value),
        "dimension" => ("x:Double", StripUnit(value, "px")),
        "number" => ("x:Double", ParseDecimal(value).ToString(CultureInfo.InvariantCulture)),
        "fontFamily" => ("FontFamily", value),
        "fontWeight" => ("FontWeight", value),
        "duration" => ("x:TimeSpan", ParseDuration(value).ToString("c", CultureInfo.InvariantCulture)),
        "string" => ("x:String", value),
        _ => throw new InvalidOperationException($"Unsupported token type '{type}'.")
    };

    private static string RenderCSharp(IEnumerable<TokenValue> tokens)
    {
        var output = new StringBuilder();
        output.AppendLine("// <auto-generated />");
        output.AppendLine("namespace Spotledger.Client.Theme;");
        output.AppendLine();
        output.AppendLine("public static class Tokens");
        output.AppendLine("{");
        foreach (var token in tokens)
        {
            output.Append("    public const string ").Append(Identifier(token.ResourceKey)).Append(" = \"")
                .Append(token.ResourceKey).AppendLine("\";");
        }

        output.AppendLine("}");
        return output.ToString();
    }

    private static string Identifier(string resourceKey) => "Token" + string.Join("", resourceKey.Split('.')
        .Select(Pascal).Select(part => new string(part.Where(char.IsLetterOrDigit).ToArray())));

    private static string Pascal(string segment) => segment.Length == 0
        ? segment
        : char.ToUpperInvariant(segment[0]) + segment[1..];

    private static string StripUnit(string value, string unit) =>
        ParseDecimal(value.EndsWith(unit, StringComparison.Ordinal) ? value[..^unit.Length] : value).ToString(CultureInfo.InvariantCulture);

    private static decimal ParseDecimal(string value) => decimal.Parse(value, NumberStyles.Float, CultureInfo.InvariantCulture);

    private static TimeSpan ParseDuration(string value) => value.EndsWith("ms", StringComparison.Ordinal)
        ? TimeSpan.FromMilliseconds((double)ParseDecimal(value[..^2]))
        : value.EndsWith('s')
            ? TimeSpan.FromSeconds((double)ParseDecimal(value[..^1]))
            : throw new InvalidOperationException($"Duration '{value}' must use ms or s.");

    private static string Xml(string value) => value.Replace("&", "&amp;", StringComparison.Ordinal)
        .Replace("<", "&lt;", StringComparison.Ordinal)
        .Replace(">", "&gt;", StringComparison.Ordinal)
        .Replace("\"", "&quot;", StringComparison.Ordinal)
        .Replace("'", "&apos;", StringComparison.Ordinal);

    private sealed record TokenValue(string ResourceKey, string Type, string Value, string? Theme);
}

internal static class Program
{
    private static int Main(string[] args)
    {
        if (args.Length != 1 || args[0] is not ("--write" or "--check"))
        {
            Console.Error.WriteLine("Usage: Spotledger.TokenGenerator --write|--check");
            return 2;
        }

        var designDirectory = FindDesignDirectory();
        var tokensPath = Path.Combine(designDirectory, "tokens.json");
        var themeDirectory = Path.GetFullPath(Path.Combine(designDirectory, "..", "src", "Spotledger.Client.Theme"));
        var axamlPath = Path.Combine(themeDirectory, "Tokens.axaml");
        var csharpPath = Path.Combine(themeDirectory, "Tokens.cs");
        var generated = TokenGenerator.Generate(File.ReadAllText(tokensPath));

        if (args[0] == "--write")
        {
            Directory.CreateDirectory(themeDirectory);
            File.WriteAllText(axamlPath, generated.Axaml, new UTF8Encoding(encoderShouldEmitUTF8Identifier: false));
            File.WriteAllText(csharpPath, generated.CSharp, new UTF8Encoding(encoderShouldEmitUTF8Identifier: false));
            Console.WriteLine("Generated theme token files.");
            return 0;
        }

        var staleFiles = new List<string>();
        if (!File.Exists(axamlPath) || !MatchesGenerated(File.ReadAllText(axamlPath), generated.Axaml))
        {
            staleFiles.Add("src/Spotledger.Client.Theme/Tokens.axaml");
        }

        if (!File.Exists(csharpPath) || !MatchesGenerated(File.ReadAllText(csharpPath), generated.CSharp))
        {
            staleFiles.Add("src/Spotledger.Client.Theme/Tokens.cs");
        }

        if (staleFiles.Count > 0)
        {
            Console.Error.WriteLine($"Generated token files are stale: {string.Join(", ", staleFiles)}. Run the generator with --write.");
            return 1;
        }

        Console.WriteLine("Generated token files are up to date.");
        return 0;
    }

    private static string FindDesignDirectory()
    {
        for (DirectoryInfo? directory = new DirectoryInfo(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
        {
            var candidate = Path.Combine(directory.FullName, "tokens.json");
            if (File.Exists(candidate))
            {
                return directory.FullName;
            }
        }

        throw new DirectoryNotFoundException("Could not find design/tokens.json from the generator output directory.");
    }

    private static bool MatchesGenerated(string onDisk, string generated) =>
        string.Equals(onDisk.ReplaceLineEndings(), generated.ReplaceLineEndings(), StringComparison.Ordinal);
}
