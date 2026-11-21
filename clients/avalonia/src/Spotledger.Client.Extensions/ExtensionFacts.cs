using System.Text.Json;

namespace Spotledger.Client.Extensions;

/// <summary>
/// What the site's manifest and the user's permission summary declare. The resolved
/// document the client receives has already been checked against these server-side
/// (WP-UI-13.T2), but the client re-checks every reference before it renders anything:
/// a hostile or stale document must not reach the dispatcher with a method, destination,
/// query or route the manifest never declared. Absent facts mean "nothing is declared",
/// so a document parsed without facts renders only what needs no declaration.
/// </summary>
public sealed class ExtensionFacts
{
    /// <summary>No declarations at all.</summary>
    public static ExtensionFacts None { get; } = new();

    /// <summary>True when the host actually declared this site's facts (a manifest was read).
    /// With no facts the client cannot re-check references: it renders the structural rules
    /// only, and the server still refuses anything its manifest never declared.</summary>
    public bool IsKnown { get; init; }

    /// <summary>Declared methods, by DocType.</summary>
    public IReadOnlyDictionary<string, IReadOnlySet<string>> Methods { get; init; } =
        new Dictionary<string, IReadOnlySet<string>>(StringComparer.Ordinal);

    /// <summary>Declared queries, by id, with their kind (<c>rows</c> or <c>aggregate</c>).</summary>
    public IReadOnlyDictionary<string, string> Queries { get; init; } =
        new Dictionary<string, string>(StringComparer.Ordinal);

    /// <summary>Declared permissions, <c>doctype:right</c>.</summary>
    public IReadOnlySet<string> Permissions { get; init; } = new HashSet<string>(StringComparer.Ordinal);

    /// <summary>Registered destination ids.</summary>
    public IReadOnlySet<string> Destinations { get; init; } = new HashSet<string>(StringComparer.Ordinal);

    /// <summary>Route prefixes the app declares, e.g. <c>/sample/reports</c>.</summary>
    public IReadOnlySet<string> Routes { get; init; } = new HashSet<string>(StringComparer.Ordinal);

    /// <summary>True when <paramref name="doctype"/>.<paramref name="method"/> is declared.</summary>
    public bool DeclaresMethod(string doctype, string method) =>
        Methods.TryGetValue(doctype, out IReadOnlySet<string>? methods) && methods.Contains(method);

    /// <summary>The kind of a declared query, or null when it is not declared.</summary>
    public string? QueryKind(string query) => Queries.TryGetValue(query, out string? kind) ? kind : null;

    /// <summary>True when some declared route covers <paramref name="path"/>.</summary>
    public bool CoversRoute(string path)
    {
        foreach (string route in Routes)
        {
            if (string.Equals(path, route, StringComparison.Ordinal) ||
                path.StartsWith(route + "/", StringComparison.Ordinal))
            {
                return true;
            }
        }
        return false;
    }

    /// <summary>
    /// Reads the shape of <c>contracts/ui-ext/examples/sample-app/context.json</c>: the
    /// manifest facts that stand in for what a site declares today. Never throws on a
    /// malformed document: an unknown shape yields <see cref="None"/>.
    /// </summary>
    public static ExtensionFacts FromContextJson(string json)
    {
        ArgumentNullException.ThrowIfNull(json);
        try
        {
            using JsonDocument document = JsonDocument.Parse(json);
            JsonElement root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                return None;
            }
            Dictionary<string, IReadOnlySet<string>> methods = new(StringComparer.Ordinal);
            if (root.TryGetProperty("methods", out JsonElement methodElement) && methodElement.ValueKind == JsonValueKind.Object)
            {
                foreach (JsonProperty property in methodElement.EnumerateObject())
                {
                    HashSet<string> names = new(StringComparer.Ordinal);
                    if (property.Value.ValueKind == JsonValueKind.Array)
                    {
                        foreach (JsonElement name in property.Value.EnumerateArray())
                        {
                            if (name.ValueKind == JsonValueKind.String)
                            {
                                names.Add(name.GetString()!);
                            }
                        }
                    }
                    methods[property.Name] = names;
                }
            }
            Dictionary<string, string> queries = new(StringComparer.Ordinal);
            if (root.TryGetProperty("queries", out JsonElement queryElement) && queryElement.ValueKind == JsonValueKind.Object)
            {
                foreach (JsonProperty property in queryElement.EnumerateObject())
                {
                    if (property.Value.ValueKind == JsonValueKind.Object &&
                        property.Value.TryGetProperty("kind", out JsonElement kind) &&
                        kind.ValueKind == JsonValueKind.String)
                    {
                        queries[property.Name] = kind.GetString()!;
                    }
                }
            }
            return new ExtensionFacts
            {
                IsKnown = true,
                Methods = methods,
                Queries = queries,
                Permissions = ReadStrings(root, "permissions"),
                Destinations = ReadStrings(root, "destinations"),
                Routes = ReadStrings(root, "routes")
            };
        }
        catch (JsonException)
        {
            return None;
        }
    }

    private static HashSet<string> ReadStrings(JsonElement root, string name)
    {
        HashSet<string> values = new(StringComparer.Ordinal);
        if (root.TryGetProperty(name, out JsonElement element) && element.ValueKind == JsonValueKind.Array)
        {
            foreach (JsonElement item in element.EnumerateArray())
            {
                if (item.ValueKind == JsonValueKind.String)
                {
                    values.Add(item.GetString()!);
                }
            }
        }
        return values;
    }
}
