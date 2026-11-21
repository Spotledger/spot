using System.Globalization;
using System.Text.Json;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>
/// Chooses the component for a form field. A view names one with a <see cref="ComponentReference"/>;
/// with no reference the host picks the installed component that renders the field's type, by id so
/// the choice is the same on every machine.
/// <para>
/// The props a reference carries are checked against the component's own props schema before an
/// instance exists, so a bad prop is a specific refusal on the form's log rather than a broken
/// control. A component from the host's built-in
/// <see cref="ComponentRegistry"/> may be chosen too; a package is preferred, because the tenant
/// installed it for this field.
/// </para>
/// </summary>
public sealed class ComponentSlotResolver
{
    private readonly ComponentCatalogue _catalogue;
    private readonly ComponentRegistry? _builtIns;

    /// <summary>Creates a resolver over the installed packages and, optionally, the built-in registry.</summary>
    /// <param name="catalogue">The installed components.</param>
    /// <param name="builtIns">The contract registry of host-built components.</param>
    public ComponentSlotResolver(ComponentCatalogue catalogue, ComponentRegistry? builtIns = null)
    {
        _catalogue = catalogue ?? throw new ArgumentNullException(nameof(catalogue));
        _builtIns = builtIns;
    }

    /// <summary>Resolves the component for <paramref name="field"/>.</summary>
    /// <param name="field">The field the slot binds to.</param>
    /// <param name="reference">The view's reference, or null to choose by field type.</param>
    public ComponentResolution Resolve(FieldMetadata field, ComponentReference? reference = null)
    {
        ArgumentNullException.ThrowIfNull(field);

        if (reference is not null)
        {
            return ResolveNamed(field, reference);
        }

        var candidates = Installed()
            .Where(c => c.Descriptor.Supports(field.Type))
            .OrderBy(c => c.Descriptor.Id, StringComparer.Ordinal)
            .ToArray();
        if (candidates.Length > 0)
        {
            return ResolveNamed(field, ComponentReference.Create(candidates[0].Descriptor.Id));
        }

        var builtIn = BuiltIns()
            .Where(d => d.Supports(field.Type))
            .OrderBy(d => d.Id, StringComparer.Ordinal)
            .FirstOrDefault();
        if (builtIn is not null)
        {
            return ComponentResolution.Accept(new ResolvedComponent(
                builtIn,
                new Dictionary<string, ComponentValue>(StringComparer.Ordinal),
                null));
        }

        return ComponentResolution.Refuse(
            ComponentRefusalCodes.ComponentUnknown,
            "no installed component renders " + field.Type.ToWireName() + " fields, and the view names none.");
    }

    private ComponentResolution ResolveNamed(FieldMetadata field, ComponentReference reference)
    {
        ComponentDescriptor? descriptor = null;
        LoadedComponent? installed = null;
        if (_catalogue.TryResolve(reference.ComponentId, out var component) && component is not null)
        {
            descriptor = component.Descriptor;
            installed = component;
        }
        else if (_builtIns?.TryResolve(reference.ComponentId, out var builtIn) == true && builtIn is not null)
        {
            descriptor = builtIn;
        }

        if (descriptor is null)
        {
            return ComponentResolution.Refuse(
                ComponentRefusalCodes.ComponentUnknown,
                "'" + reference.ComponentId + "' is not installed and is not a built-in component.");
        }

        if (!descriptor.Supports(field.Type))
        {
            return ComponentResolution.Refuse(
                ComponentRefusalCodes.PropsRefused,
                "'" + descriptor.Id + "' does not render " + field.Type.ToWireName() + " fields.");
        }

        var schema = descriptor.PropsSchema;
        if (schema is null)
        {
            return ComponentResolution.Refuse(
                ComponentRefusalCodes.PropsRefused,
                "'" + descriptor.Id + "' ships no readable props schema, so its props cannot be checked.");
        }

        var props = new Dictionary<string, ComponentValue>(StringComparer.Ordinal);
        foreach (var (name, value) in reference.Props)
        {
            var property = schema.Properties.FirstOrDefault(p => string.Equals(p.Name, name, StringComparison.Ordinal));
            if (property is null)
            {
                return ComponentResolution.Refuse(
                    ComponentRefusalCodes.PropsRefused,
                    "'" + descriptor.Id + "' declares no prop '" + name + "'.");
            }

            if (!TryReadProp(value, property, out var componentValue))
            {
                return ComponentResolution.Refuse(
                    ComponentRefusalCodes.PropsRefused,
                    "prop '" + name + "' of '" + descriptor.Id + "' is not a "
                    + property.FieldType.ToWireName() + " value.");
            }

            props[name] = componentValue;
        }

        var problems = schema.Validate(props);
        if (problems.Any(static m => m.IsError))
        {
            return ComponentResolution.Refuse(
                ComponentRefusalCodes.PropsRefused,
                string.Join(" ", problems.Select(static m => m.Code + ": " + m.Message)));
        }

        return ComponentResolution.Accept(new ResolvedComponent(descriptor, props, installed));
    }

    private IEnumerable<LoadedComponent> Installed() => _catalogue.Components;

    private IEnumerable<ComponentDescriptor> BuiltIns() => _builtIns?.Components ?? Enumerable.Empty<ComponentDescriptor>();

    private static bool TryReadProp(JsonElement value, PropsSchemaProperty property, out ComponentValue prop)
    {
        prop = ComponentValue.Null(property.FieldType);
        switch (property.FieldType)
        {
            case ComponentFieldType.Boolean when value.ValueKind is JsonValueKind.True or JsonValueKind.False:
                prop = ComponentValue.FromBoolean(value.GetBoolean());
                return true;
            case ComponentFieldType.IntegerNumber when value.ValueKind == JsonValueKind.Number
                && value.TryGetInt64(out var integer):
                prop = ComponentValue.FromInteger(integer);
                return true;
            case ComponentFieldType.DecimalNumber when value.ValueKind == JsonValueKind.Number
                && value.TryGetDecimal(out var exact):
                prop = ComponentValue.FromDecimal(exact);
                return true;
            case ComponentFieldType.Table when value.ValueKind == JsonValueKind.Array:
                prop = ComponentValue.FromJson(value.GetRawText());
                return true;
            case ComponentFieldType.Json when value.ValueKind is JsonValueKind.Object or JsonValueKind.Array:
                prop = ComponentValue.FromJson(value.GetRawText());
                return true;
            case ComponentFieldType.Text or ComponentFieldType.LongText or ComponentFieldType.Date
                or ComponentFieldType.Time or ComponentFieldType.Link or ComponentFieldType.Select
                or ComponentFieldType.Bytes or ComponentFieldType.Attach or ComponentFieldType.Secret
                when value.ValueKind == JsonValueKind.String:
                prop = ComponentValue.FromString(property.FieldType, value.GetString() ?? string.Empty);
                return true;
            default:
                return false;
        }
    }

    /// <inheritdoc />
    public override string ToString() =>
        (_builtIns?.Components.Count ?? 0).ToString(CultureInfo.InvariantCulture) + " built-in and "
        + _catalogue.Count.ToString(CultureInfo.InvariantCulture) + " installed component(s)";
}
