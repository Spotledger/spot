using System.Text.Json;

namespace Spotledger.Component.Host;

/// <summary>
/// A view's reference to a tier B component: the component's id and the props the view sets.
/// <para>
/// The reference's JSON shape is <c>{"component_id": "acme.rating", "props": {"max": 5}}</c>. There is
/// no tier B reference in <c>contracts/ui-ext</c> or in <c>spot-views</c> yet (both are tier A), so
/// this is the smallest seam that lets a view name a component today: a string a view definition can
/// carry, parsed here and validated against the component's own props schema before anything is
/// created. It is deliberately not a contract change.
/// </para>
/// </summary>
public sealed class ComponentReference
{
    private ComponentReference(string componentId, IReadOnlyDictionary<string, JsonElement> props)
    {
        ComponentId = componentId;
        Props = props;
    }

    /// <summary>The component's id.</summary>
    public string ComponentId { get; }

    /// <summary>The props the view sets, as detached JSON values (they outlive the document).</summary>
    public IReadOnlyDictionary<string, JsonElement> Props { get; }

    /// <summary>Creates a reference from an id and props.</summary>
    /// <param name="componentId">The component id.</param>
    /// <param name="props">The props; null for none.</param>
    public static ComponentReference Create(string componentId, IReadOnlyDictionary<string, JsonElement>? props = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(componentId);
        return new ComponentReference(
            componentId,
            props ?? new Dictionary<string, JsonElement>(StringComparer.Ordinal));
    }

    /// <summary>Parses a view's component reference, reporting every problem instead of throwing.</summary>
    /// <param name="json">The reference text.</param>
    /// <param name="reference">The parsed reference, when it is sound.</param>
    /// <param name="problems">What was wrong; empty when it parsed.</param>
    public static bool TryParse(string? json, out ComponentReference? reference, out IReadOnlyList<string> problems)
    {
        reference = null;
        if (string.IsNullOrWhiteSpace(json))
        {
            problems = new[] { "component reference is empty." };
            return false;
        }

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(json);
        }
        catch (JsonException error)
        {
            problems = new[] { "component reference is not JSON: " + error.Message };
            return false;
        }

        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                problems = new[] { "component reference is an object." };
                return false;
            }

            var found = new List<string>();
            string? componentId = null;
            var props = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
            foreach (var member in root.EnumerateObject())
            {
                switch (member.Name)
                {
                    case "component_id" when member.Value.ValueKind == JsonValueKind.String:
                        componentId = member.Value.GetString();
                        break;
                    case "component_id":
                        found.Add("component reference: component_id is a string.");
                        break;
                    case "props" when member.Value.ValueKind == JsonValueKind.Object:
                        foreach (var prop in member.Value.EnumerateObject())
                        {
                            props[prop.Name] = prop.Value.Clone();
                        }

                        break;
                    case "props":
                        found.Add("component reference: props is an object.");
                        break;
                    default:
                        found.Add("component reference." + member.Name + ": no such member.");
                        break;
                }
            }

            if (string.IsNullOrWhiteSpace(componentId))
            {
                found.Add("component reference: component_id is required.");
            }

            problems = found;
            if (found.Count > 0 || componentId is null)
            {
                return false;
            }

            reference = new ComponentReference(componentId, props);
            return true;
        }
    }

    /// <inheritdoc />
    public override string ToString() => ComponentId + " (" + Props.Count + " prop(s))";
}

/// <summary>What a view slot resolved to: the descriptor and the props, ready to create.</summary>
/// <param name="Descriptor">The chosen component.</param>
/// <param name="Props">The validated props.</param>
/// <param name="Installed">The installed package the descriptor came from; null for a built-in.</param>
public sealed record ResolvedComponent(
    Contract.ComponentDescriptor Descriptor,
    IReadOnlyDictionary<string, Contract.ComponentValue> Props,
    LoadedComponent? Installed);

/// <summary>The outcome of resolving a field to a component: the choice, or why there is none.</summary>
public sealed class ComponentResolution
{
    private ComponentResolution(ResolvedComponent? resolved, ComponentRefusal? refusal)
    {
        Resolved = resolved;
        Refusal = refusal;
    }

    /// <summary>The resolved component; null when nothing fits.</summary>
    public ResolvedComponent? Resolved { get; }

    /// <summary>The refusal; null when a component was chosen.</summary>
    public ComponentRefusal? Refusal { get; }

    /// <summary>True when a component was chosen.</summary>
    public bool IsResolved => Resolved is not null;

    /// <summary>A resolution.</summary>
    public static ComponentResolution Accept(ResolvedComponent resolved) =>
        new(resolved ?? throw new ArgumentNullException(nameof(resolved)), null);

    /// <summary>A refusal.</summary>
    public static ComponentResolution Refuse(string code, string message) =>
        new(null, new ComponentRefusal(code, message));
}
