using System.Globalization;
using System.Text.Json;

namespace Spotledger.Component.Contract;

/// <summary>
/// One prop of a component's JSON Schema: its name, the field type it binds to, the JSON type
/// JSON Schema requires, and the constraints the kit enforces on values.
/// </summary>
/// <param name="Name">The prop name.</param>
/// <param name="FieldType">The field type it binds to.</param>
/// <param name="JsonType">The JSON Schema type (<c>boolean</c>, <c>integer</c>, <c>number</c>, <c>string</c>, <c>array</c>, <c>object</c>).</param>
/// <param name="Required">Whether the prop must be supplied.</param>
/// <param name="AllowedValues">The <c>enum</c> members, when declared.</param>
/// <param name="Minimum">The inclusive numeric minimum, when declared.</param>
/// <param name="Maximum">The inclusive numeric maximum, when declared.</param>
/// <param name="MaxLength">The string length limit, when declared.</param>
/// <param name="Description">The prop's human description.</param>
public sealed record PropsSchemaProperty(
    string Name,
    ComponentFieldType FieldType,
    string JsonType,
    bool Required,
    IReadOnlyList<string>? AllowedValues,
    decimal? Minimum,
    decimal? Maximum,
    int? MaxLength,
    string? Description);

/// <summary>
/// The props schema a descriptor ships: a JSON Schema (draft 2020-12) document restricted to the
/// subset the contract can enforce without a validator dependency — object root, typed props,
/// <c>required</c>, <c>enum</c>, <c>minimum</c>/<c>maximum</c> and <c>maxLength</c>, plus the
/// <c>x-field-type</c> annotation that binds a prop to the field vocabulary.
/// </summary>
public sealed class PropsSchema
{
    private readonly Dictionary<string, PropsSchemaProperty> _byName;

    private PropsSchema(string raw, IReadOnlyList<PropsSchemaProperty> properties)
    {
        Raw = raw;
        Properties = properties;
        _byName = new Dictionary<string, PropsSchemaProperty>(StringComparer.Ordinal);
        foreach (var property in properties)
        {
            _byName[property.Name] = property;
        }
    }

    /// <summary>The JSON Schema draft this contract speaks.</summary>
    public const string Draft = "https://json-schema.org/draft/2020-12/schema";

    /// <summary>The document as shipped, for diagnostics and the store's review report.</summary>
    public string Raw { get; }

    /// <summary>The declared props, in document order.</summary>
    public IReadOnlyList<PropsSchemaProperty> Properties { get; }

    /// <summary>Parses a schema document, reporting every problem instead of throwing.</summary>
    public static bool TryParse(string? json, out PropsSchema? schema, out IReadOnlyList<string> problems)
    {
        schema = null;
        var found = new List<string>();
        if (string.IsNullOrWhiteSpace(json))
        {
            problems = new[] { "props.schema.missing: the descriptor ships no props schema." };
            return false;
        }

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(json);
        }
        catch (JsonException error)
        {
            problems = new[] { "props.schema.invalid_json: " + error.Message };
            return false;
        }

        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                problems = new[] { "props.schema.root: the schema's root is an object." };
                return false;
            }

            if (root.TryGetProperty("$schema", out var draft)
                && !string.Equals(draft.GetString(), Draft, StringComparison.Ordinal))
            {
                found.Add("props.schema.draft: $schema must be '" + Draft + "'.");
            }

            if (!root.TryGetProperty("type", out var rootType)
                || !string.Equals(rootType.GetString(), "object", StringComparison.Ordinal))
            {
                found.Add("props.schema.root: the schema's root type is 'object'.");
            }

            CheckKeywords(root, string.Empty, isRoot: true, depth: 0, found);

            var required = ReadRequired(root, found);
            var properties = ReadProperties(root, required, found);
            if (properties.Count == 0)
            {
                found.Add("props.schema.empty: the schema declares no properties.");
            }

            problems = found;
            if (found.Count > 0)
            {
                return false;
            }

            schema = new PropsSchema(json, properties);
            return true;
        }
    }

    /// <summary>
    /// The keywords the root of a schema may use. Anything else is refused, because a keyword the
    /// parser does not enforce would let a component believe it validated something it did not.
    /// <c>additionalProperties</c> is accepted only as <c>false</c>, which <see cref="Validate"/>
    /// always enforces (an undeclared prop is refused).
    /// </summary>
    private static readonly HashSet<string> RootKeywords = new(StringComparer.Ordinal)
    {
        "$schema", "type", "properties", "required", "additionalProperties", "title", "description", "default",
    };

    /// <summary>
    /// The keywords a property schema, or the schema under <c>items</c>, may use. Constraints
    /// beyond these (<c>$ref</c>, <c>oneOf</c>, <c>anyOf</c>, <c>allOf</c>, <c>not</c>,
    /// <c>patternProperties</c>, <c>if</c>/<c>then</c>, <c>format</c>, <c>pattern</c> ...) are not
    /// enforced, so they are reported instead of dropped.
    /// </summary>
    private static readonly HashSet<string> PropertyKeywords = new(StringComparer.Ordinal)
    {
        "type", "x-field-type", "enum", "minimum", "maximum", "maxLength", "items", "title", "description", "default",
    };

    private const int MaxKeywordDepth = 8;

    private static void CheckKeywords(JsonElement schema, string pointer, bool isRoot, int depth, List<string> problems)
    {
        var allowed = isRoot ? RootKeywords : PropertyKeywords;
        foreach (var member in schema.EnumerateObject())
        {
            if (!allowed.Contains(member.Name))
            {
                problems.Add("props.schema.unsupported_keyword: '" + member.Name + "' at "
                    + pointer + "/" + EscapePointer(member.Name) + " is not supported, so it would not be enforced.");
                continue;
            }

            if (isRoot && string.Equals(member.Name, "additionalProperties", StringComparison.Ordinal)
                && member.Value.ValueKind != JsonValueKind.False)
            {
                problems.Add("props.schema.unsupported_keyword: 'additionalProperties' at "
                    + pointer + "/additionalProperties is supported only as false.");
            }
        }

        if (depth >= MaxKeywordDepth)
        {
            // Not walked any deeper, so a keyword below here would go unchecked: refuse the schema
            // rather than accept what was not inspected.
            if (!isRoot && schema.TryGetProperty("items", out var deeper) && deeper.ValueKind == JsonValueKind.Object)
            {
                problems.Add("props.schema.too_deep: '" + pointer + "/items' is nested deeper than "
                    + MaxKeywordDepth.ToString(System.Globalization.CultureInfo.InvariantCulture)
                    + " levels, so its keywords are not checked.");
            }

            return;
        }

        if (isRoot && schema.TryGetProperty("properties", out var declared) && declared.ValueKind == JsonValueKind.Object)
        {
            foreach (var entry in declared.EnumerateObject())
            {
                if (entry.Value.ValueKind == JsonValueKind.Object)
                {
                    CheckKeywords(entry.Value, pointer + "/properties/" + EscapePointer(entry.Name), false, depth + 1, problems);
                }
            }
        }

        if (!isRoot && schema.TryGetProperty("items", out var items) && items.ValueKind == JsonValueKind.Object)
        {
            CheckKeywords(items, pointer + "/items", false, depth + 1, problems);
        }
    }

    private static string EscapePointer(string token) =>
        token.Replace("~", "~0", StringComparison.Ordinal).Replace("/", "~1", StringComparison.Ordinal);

    /// <summary>
    /// Every prop's field type must be one the descriptor says it supports; otherwise the
    /// component could be bound to a field it never claimed to render.
    /// </summary>
    public IReadOnlyList<string> ValidateAgainstSupportedTypes(IReadOnlyCollection<ComponentFieldType> supportedFieldTypes)
    {
        ArgumentNullException.ThrowIfNull(supportedFieldTypes);
        var problems = new List<string>();
        foreach (var property in Properties)
        {
            if (!supportedFieldTypes.Contains(property.FieldType))
            {
                problems.Add("props.schema.unsupported: prop '" + property.Name + "' binds to "
                    + property.FieldType.ToWireName() + ", which the descriptor does not list as supported.");
            }
        }

        return problems;
    }

    /// <summary>Validates a prop set: required props present, types and constraints honoured, nothing unknown.</summary>
    public IReadOnlyList<ValidationMessage> Validate(IReadOnlyDictionary<string, ComponentValue> props)
    {
        ArgumentNullException.ThrowIfNull(props);
        var messages = new List<ValidationMessage>();
        foreach (var property in Properties)
        {
            if (!props.TryGetValue(property.Name, out var value) || value.IsNull)
            {
                if (property.Required)
                {
                    messages.Add(new ValidationMessage(
                        "props.required",
                        "Prop '" + property.Name + "' is required."));
                }

                continue;
            }

            if (value.Type != property.FieldType)
            {
                messages.Add(new ValidationMessage(
                    "props.type",
                    "Prop '" + property.Name + "' is " + property.FieldType.ToWireName()
                    + ", not " + value.Type.ToWireName() + "."));
                continue;
            }

            CheckConstraints(property, value, messages);
        }

        foreach (var name in props.Keys)
        {
            if (!_byName.ContainsKey(name))
            {
                messages.Add(new ValidationMessage("props.unknown", "Prop '" + name + "' is not declared."));
            }
        }

        return messages;
    }

    private static void CheckConstraints(PropsSchemaProperty property, ComponentValue value, List<ValidationMessage> messages)
    {
        if (property.AllowedValues is { Count: > 0 } allowed
            && (value.Text is null || !allowed.Contains(value.Text, StringComparer.Ordinal)))
        {
            messages.Add(new ValidationMessage(
                "props.enum",
                "Prop '" + property.Name + "' must be one of: " + string.Join(", ", allowed) + "."));
        }

        if (property.MaxLength is { } maxLength && value.Text is { Length: > 0 } text && text.Length > maxLength)
        {
            messages.Add(new ValidationMessage(
                "props.max_length",
                "Prop '" + property.Name + "' is longer than " + maxLength.ToString(CultureInfo.InvariantCulture) + " characters."));
        }

        if (property.Minimum is null && property.Maximum is null)
        {
            return;
        }

        if (!TryReadNumber(value, out var number))
        {
            return;
        }

        if (property.Minimum is { } minimum && number < minimum)
        {
            messages.Add(new ValidationMessage(
                "props.minimum",
                "Prop '" + property.Name + "' must be at least " + minimum.ToString(CultureInfo.InvariantCulture) + "."));
        }

        if (property.Maximum is { } maximum && number > maximum)
        {
            messages.Add(new ValidationMessage(
                "props.maximum",
                "Prop '" + property.Name + "' must be at most " + maximum.ToString(CultureInfo.InvariantCulture) + "."));
        }
    }

    private static bool TryReadNumber(ComponentValue value, out decimal number)
    {
        number = decimal.Zero;
        return value.Type switch
        {
            ComponentFieldType.DecimalNumber => value.TryGetDecimal(out number),
            ComponentFieldType.IntegerNumber => value.TryGetInteger(out var integer) && (number = integer) == integer,
            _ => false,
        };
    }

    private static HashSet<string> ReadRequired(JsonElement root, List<string> problems)
    {
        var required = new HashSet<string>(StringComparer.Ordinal);
        if (!root.TryGetProperty("required", out var array))
        {
            return required;
        }

        if (array.ValueKind != JsonValueKind.Array)
        {
            problems.Add("props.schema.required: 'required' is an array of prop names.");
            return required;
        }

        foreach (var item in array.EnumerateArray())
        {
            if (item.ValueKind != JsonValueKind.String || string.IsNullOrWhiteSpace(item.GetString()))
            {
                problems.Add("props.schema.required: every 'required' entry is a prop name.");
                continue;
            }

            required.Add(item.GetString()!);
        }

        return required;
    }

    private static List<PropsSchemaProperty> ReadProperties(JsonElement root, HashSet<string> required, List<string> problems)
    {
        var properties = new List<PropsSchemaProperty>();
        if (!root.TryGetProperty("properties", out var declared))
        {
            problems.Add("props.schema.properties: the schema declares no 'properties' object.");
            return properties;
        }

        if (declared.ValueKind != JsonValueKind.Object)
        {
            problems.Add("props.schema.properties: 'properties' is an object.");
            return properties;
        }

        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var entry in declared.EnumerateObject())
        {
            var name = entry.Name;
            if (string.IsNullOrWhiteSpace(name))
            {
                problems.Add("props.schema.name: a property has an empty name.");
                continue;
            }

            if (!seen.Add(name))
            {
                problems.Add("props.schema.duplicate: property '" + name + "' is declared twice.");
                continue;
            }

            if (entry.Value.ValueKind != JsonValueKind.Object)
            {
                problems.Add("props.schema.property: property '" + name + "' is an object.");
                continue;
            }

            var jsonType = entry.Value.TryGetProperty("type", out var typeElement) && typeElement.ValueKind == JsonValueKind.String
                ? typeElement.GetString()!
                : null;
            if (jsonType is null || !KnownJsonTypes.Contains(jsonType))
            {
                problems.Add("props.schema.type: property '" + name + "' needs a supported 'type'.");
                continue;
            }

            if (!TryReadFieldType(name, entry.Value, jsonType, problems, out var fieldType))
            {
                continue;
            }

            properties.Add(new PropsSchemaProperty(
                name,
                fieldType,
                jsonType,
                required.Contains(name),
                ReadStringArray(name, entry.Value, "enum", problems),
                ReadDecimal(name, entry.Value, "minimum", problems),
                ReadDecimal(name, entry.Value, "maximum", problems),
                ReadInt(name, entry.Value, "maxLength", problems),
                entry.Value.TryGetProperty("description", out var description) && description.ValueKind == JsonValueKind.String
                    ? description.GetString()
                    : null));
        }

        foreach (var name in required)
        {
            if (!seen.Contains(name))
            {
                problems.Add("props.schema.required: '" + name + "' is required but not declared.");
            }
        }

        return properties;
    }

    private static readonly string[] KnownJsonTypes = { "boolean", "integer", "number", "string", "array", "object" };

    private static bool TryReadFieldType(
        string name,
        JsonElement property,
        string jsonType,
        List<string> problems,
        out ComponentFieldType fieldType)
    {
        fieldType = default;
        ComponentFieldType? declared = null;
        if (property.TryGetProperty("x-field-type", out var annotation))
        {
            if (annotation.ValueKind != JsonValueKind.String
                || ComponentFieldTypes.TryParse(annotation.GetString()) is not { } parsed)
            {
                problems.Add("props.schema.field_type: property '" + name + "' has an unknown x-field-type.");
                return false;
            }

            declared = parsed;
        }

        var expected = JsonTypeOf(declared ?? DefaultFieldTypeFor(jsonType));
        if (!string.Equals(expected, jsonType, StringComparison.Ordinal))
        {
            problems.Add("props.schema.field_type: property '" + name + "' binds x-field-type "
                + (declared ?? DefaultFieldTypeFor(jsonType)).ToWireName() + " to JSON type '" + jsonType + "'.");
            return false;
        }

        fieldType = declared ?? DefaultFieldTypeFor(jsonType);
        return true;
    }

    private static ComponentFieldType DefaultFieldTypeFor(string jsonType) => jsonType switch
    {
        "boolean" => ComponentFieldType.Boolean,
        "integer" => ComponentFieldType.IntegerNumber,
        "number" => ComponentFieldType.DecimalNumber,
        "array" => ComponentFieldType.Table,
        "object" => ComponentFieldType.Json,
        _ => ComponentFieldType.Text,
    };

    private static string JsonTypeOf(ComponentFieldType fieldType) => fieldType switch
    {
        ComponentFieldType.Boolean => "boolean",
        ComponentFieldType.IntegerNumber => "integer",
        ComponentFieldType.DecimalNumber => "number",
        ComponentFieldType.Table => "array",
        ComponentFieldType.Json => "object",
        _ => "string",
    };

    private static List<string>? ReadStringArray(string name, JsonElement property, string member, List<string> problems)
    {
        if (!property.TryGetProperty(member, out var array))
        {
            return null;
        }

        if (array.ValueKind != JsonValueKind.Array)
        {
            problems.Add("props.schema." + member + ": property '" + name + "' declares '" + member + "' as an array.");
            return null;
        }

        var values = new List<string>();
        foreach (var item in array.EnumerateArray())
        {
            if (item.ValueKind != JsonValueKind.String)
            {
                problems.Add("props.schema." + member + ": property '" + name + "' has a non-string '" + member + "' entry.");
                return null;
            }

            values.Add(item.GetString()!);
        }

        return values;
    }

    private static decimal? ReadDecimal(string name, JsonElement property, string member, List<string> problems)
    {
        if (!property.TryGetProperty(member, out var element))
        {
            return null;
        }

        if (element.ValueKind != JsonValueKind.Number || !element.TryGetDecimal(out var value))
        {
            problems.Add("props.schema." + member + ": property '" + name + "' declares a non-numeric '" + member + "'.");
            return null;
        }

        return value;
    }

    private static int? ReadInt(string name, JsonElement property, string member, List<string> problems)
    {
        if (!property.TryGetProperty(member, out var element))
        {
            return null;
        }

        if (element.ValueKind != JsonValueKind.Number || !element.TryGetInt32(out var value))
        {
            problems.Add("props.schema." + member + ": property '" + name + "' declares a non-integer '" + member + "'.");
            return null;
        }

        return value;
    }
}
