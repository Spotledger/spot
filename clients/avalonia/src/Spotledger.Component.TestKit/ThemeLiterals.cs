using System.Text.RegularExpressions;
using System.Xml.Linq;

namespace Spotledger.Component.TestKit;

/// <summary>
/// Reads a component's XAML and flags every styling literal: a colour (hex or named), a font
/// family, or a metric (padding, margin, size, radius, thickness, font size) written as a number
/// instead of a token reference. This is the mechanical form of the design-system rule — a
/// component that hard-codes a colour looks foreign in the other theme, so the kit refuses it.
/// </summary>
public static partial class ThemeLiterals
{
    private static readonly XNamespace AvaloniaNamespace = "https://github.com/avaloniaui";

    private static readonly HashSet<string> ColourProperties = new(StringComparer.Ordinal)
    {
        "Background", "Foreground", "BorderBrush", "CaretBrush", "SelectionBrush", "Stroke", "Fill", "TextElement.Foreground",
    };

    private static readonly HashSet<string> MetricProperties = new(StringComparer.Ordinal)
    {
        "Padding", "Margin", "Height", "Width", "MinHeight", "MinWidth", "MaxHeight", "MaxWidth",
        "CornerRadius", "BorderThickness", "FontSize", "Spacing", "ColumnSpacing", "RowSpacing",
        "Gap", "LetterSpacing", "StrokeThickness",
    };

    private static readonly HashSet<string> NamedColours = new(StringComparer.OrdinalIgnoreCase)
    {
        "Red", "Green", "Blue", "Black", "White", "Gray", "Grey", "Silver", "Yellow",
        "Orange", "Purple", "Pink", "Brown", "Cyan", "Magenta", "Gold", "Teal", "Navy", "Maroon",
    };

    /// <summary>Every literal in <paramref name="xaml"/>, as violations.</summary>
    public static IReadOnlyList<ComponentContractViolation> Scan(string xaml)
    {
        ArgumentNullException.ThrowIfNull(xaml);
        var violations = new List<ComponentContractViolation>();
        XDocument document;
        try
        {
            document = XDocument.Parse(xaml);
        }
        catch (System.Xml.XmlException error)
        {
            violations.Add(new ComponentContractViolation(ViolationCodes.ThemeMissing, "The styles are not valid XAML: " + error.Message));
            return violations;
        }

        foreach (var element in document.Descendants())
        {
            if (IsInsideThemeDictionary(element))
            {
                continue;
            }

            foreach (var (property, value) in StylingPairs(element))
            {
                if (value.Length == 0 || value.StartsWith('{'))
                {
                    continue;
                }

                if (ColourProperties.Contains(property) && LooksLikeColour(value))
                {
                    violations.Add(new ComponentContractViolation(
                        ViolationCodes.TokenLiteral,
                        PropertyName(element, property) + "=\"" + value + "\" is a hard-coded colour; use a Theme token."));
                }
                else if (string.Equals(property, "FontFamily", StringComparison.Ordinal))
                {
                    violations.Add(new ComponentContractViolation(
                        ViolationCodes.TokenLiteral,
                        PropertyName(element, property) + "=\"" + value + "\" is a hard-coded font; use Font.Family.*."));
                }
                else if (MetricProperties.Contains(property) && !IsZeroOrKeyword(value))
                {
                    violations.Add(new ComponentContractViolation(
                        ViolationCodes.TokenLiteral,
                        PropertyName(element, property) + "=\"" + value + "\" is a hard-coded metric; use a Space.*/Radius.*/Font.Size.* token."));
                }
            }
        }

        return violations;
    }

    /// <summary>The distinct token keys <paramref name="xaml"/> references.</summary>
    public static IReadOnlyCollection<string> ReferencedTokens(string xaml)
    {
        ArgumentNullException.ThrowIfNull(xaml);
        var keys = new HashSet<string>(StringComparer.Ordinal);
        foreach (var match in TokenReferencePattern().Matches(xaml).Cast<Match>())
        {
            keys.Add(match.Groups["key"].Value);
        }

        return keys;
    }

    private static IEnumerable<(string Property, string Value)> StylingPairs(XElement element)
    {
        var isSetter = string.Equals(element.Name.LocalName, "Setter", StringComparison.Ordinal);
        if (isSetter)
        {
            var property = element.Attribute("Property")?.Value;
            var value = element.Attribute("Value")?.Value;
            if (property is not null && value is not null)
            {
                // A token extension in the Setter's Value is a reference, not a literal.
                yield return (StripOwnerPrefix(property.Trim()), value.Trim());
            }

            yield break;
        }

        foreach (var attribute in element.Attributes())
        {
            if (attribute.IsNamespaceDeclaration)
            {
                continue;
            }

            yield return (attribute.Name.LocalName, attribute.Value.Trim());
        }
    }

    private static string StripOwnerPrefix(string property)
    {
        var dot = property.IndexOf('.', StringComparison.Ordinal);
        return dot >= 0 ? property[(dot + 1)..] : property;
    }

    private static bool IsInsideThemeDictionary(XElement element)
    {
        for (var current = element.Parent; current is not null; current = current.Parent)
        {
            if (string.Equals(current.Name.LocalName, "ThemeDictionaries", StringComparison.Ordinal)
                || (string.Equals(current.Name.LocalName, "ResourceDictionary", StringComparison.Ordinal)
                    && current.Attribute(XName.Get("Key", "http://schemas.microsoft.com/winfx/2006/xaml")) is not null
                    && current.Parent is not null
                    && string.Equals(current.Parent.Name.LocalName, "ThemeDictionaries", StringComparison.Ordinal)))
            {
                return true;
            }
        }

        return false;
    }

    private static string PropertyName(XElement element, string property) =>
        element.Name.LocalName + "." + property;

    private static bool LooksLikeColour(string value) =>
        value.StartsWith('#')
        || NamedColours.Contains(value)
        || value.StartsWith("rgb(", StringComparison.OrdinalIgnoreCase)
        || value.StartsWith("hsl(", StringComparison.OrdinalIgnoreCase);

    private static bool IsZeroOrKeyword(string value) =>
        string.Equals(value, "0", StringComparison.Ordinal)
        || string.Equals(value, "Auto", StringComparison.OrdinalIgnoreCase)
        || string.Equals(value, "Stretch", StringComparison.OrdinalIgnoreCase)
        || string.Equals(value, "NaN", StringComparison.OrdinalIgnoreCase)
        || string.Equals(value, "None", StringComparison.OrdinalIgnoreCase);

    [GeneratedRegex(@"\{(?:DynamicResource|StaticResource)\s+(?<key>[A-Za-z][A-Za-z0-9_.]*)\s*\}", RegexOptions.CultureInvariant)]
    private static partial Regex TokenReferencePattern();
}
