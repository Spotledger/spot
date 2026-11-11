using System.Globalization;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace Spotledger.Client.Extensions;

/// <summary>Defensive readers for a document element: every one tolerates a missing or
/// mistyped property and reports nothing, so the caller decides which code to use.</summary>
internal static class JsonRead
{
    internal static string? String(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.String
            ? value.GetString()
            : null;

    internal static bool? Bool(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind is JsonValueKind.True or JsonValueKind.False
            ? value.GetBoolean()
            : null;

    internal static int? Int(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.Number &&
        value.TryGetInt32(out int number)
            ? number
            : null;

    internal static JsonElement? Element(JsonElement element, string name, JsonValueKind kind) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == kind ? value : null;

    internal static JsonElement? Object(JsonElement element, string name) => Element(element, name, JsonValueKind.Object);

    internal static JsonElement? Array(JsonElement element, string name) => Element(element, name, JsonValueKind.Array);

    internal static bool Has(JsonElement element, string name) => element.TryGetProperty(name, out _);

    /// <summary>The element's properties, tolerating a wrong container.</summary>
    internal static IEnumerable<JsonElement> Items(JsonElement? array)
    {
        if (array is null)
        {
            return [];
        }
        return array.Value.EnumerateArray();
    }

    /// <summary>A map's values, tolerating a wrong container.</summary>
    internal static IEnumerable<(string Name, JsonElement Value)> Properties(JsonElement? map)
    {
        if (map is null)
        {
            return [];
        }
        return map.Value.EnumerateObject().Select(static property => (property.Name, property.Value));
    }

    /// <summary>The property names of an object, for closed-object checking.</summary>
    internal static IEnumerable<string> Names(JsonElement element) =>
        element.ValueKind == JsonValueKind.Object
            ? element.EnumerateObject().Select(static property => property.Name)
            : [];
}

/// <summary>The document's shapes: namespaced ids, names, permission references and origins.</summary>
internal static partial class ExtensionPatterns
{
    [GeneratedRegex("^[a-z][a-z0-9_]*$", RegexOptions.CultureInvariant)]
    internal static partial Regex Name();

    [GeneratedRegex("^[a-z][a-z0-9_]*\\.[a-z][a-z0-9_]*$", RegexOptions.CultureInvariant)]
    internal static partial Regex Id();

    [GeneratedRegex("^app:[a-z][a-z0-9_]*$", RegexOptions.CultureInvariant)]
    internal static partial Regex Origin();

    [GeneratedRegex("^[a-z][a-z0-9_]*:[a-z][a-z0-9_]*$", RegexOptions.CultureInvariant)]
    internal static partial Regex Permission();

    [GeneratedRegex("^\\d+\\.\\d+\\.\\d+$", RegexOptions.CultureInvariant)]
    internal static partial Regex Version();

    /// <summary>True when the text is a name of the contract's grammar.</summary>
    internal static bool IsName(string? text) => text is not null && text.Length <= 64 && Name().IsMatch(text);

    /// <summary>True when the text is a namespaced id of the contract's grammar.</summary>
    internal static bool IsId(string? text) => text is not null && text.Length <= 129 && Id().IsMatch(text);

    /// <summary>True when the text is an origin of the contract's grammar.</summary>
    internal static bool IsOrigin(string? text) => text is not null && Origin().IsMatch(text);

    /// <summary>True when the text is a permission reference of the contract's grammar.</summary>
    internal static bool IsPermission(string? text) => text is not null && Permission().IsMatch(text);

    /// <summary>The app of an origin (<c>app:sample</c> → <c>sample</c>), or null.</summary>
    internal static string? AppOf(string? origin)
    {
        if (!IsOrigin(origin))
        {
            return null;
        }
        string value = origin!;
        return value["app:".Length..];
    }

    /// <summary>The app namespace of an id (<c>sample.x</c> → <c>sample</c>), or null.</summary>
    internal static string? AppOfId(string? id)
    {
        if (!IsId(id))
        {
            return null;
        }
        string value = id!;
        return value[..value.IndexOf('.', StringComparison.Ordinal)];
    }

    /// <summary>True when a label is a translation key rather than a literal.</summary>
    internal static bool IsTranslationKey(string label) => label.StartsWith("key:", StringComparison.Ordinal);

    /// <summary>The length of a label, counted in Unicode scalars (the contract's unit).</summary>
    internal static int LabelLength(string label) => new System.Globalization.StringInfo(label).LengthInTextElements;

    /// <summary>Parses the major of a semver string, or null.</summary>
    internal static int? MajorOf(string version)
    {
        if (!Version().IsMatch(version))
        {
            return null;
        }
        string major = version[..version.IndexOf('.', StringComparison.Ordinal)];
        return int.TryParse(major, NumberStyles.None, CultureInfo.InvariantCulture, out int value) ? value : null;
    }

    /// <summary>Compares two semver strings numerically; null when either does not parse.</summary>
    internal static int? Compare(string left, string right)
    {
        if (!Version().IsMatch(left) || !Version().IsMatch(right))
        {
            return null;
        }
        string[] leftParts = left.Split('.');
        string[] rightParts = right.Split('.');
        for (int index = 0; index < 3; index++)
        {
            if (!int.TryParse(leftParts[index], NumberStyles.None, CultureInfo.InvariantCulture, out int a) ||
                !int.TryParse(rightParts[index], NumberStyles.None, CultureInfo.InvariantCulture, out int b))
            {
                return null;
            }
            if (a != b)
            {
                return a < b ? -1 : 1;
            }
        }
        return 0;
    }
}
