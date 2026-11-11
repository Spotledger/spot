using System.Globalization;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace Spotledger.Client.Effects.Tests.Schema;

/// <summary>
/// A tiny JSON Schema checker for the keywords the effects schema uses: <c>$ref</c> into
/// <c>$defs</c>, <c>type</c>, <c>const</c>, <c>enum</c>, <c>required</c>,
/// <c>additionalProperties: false</c>, <c>properties</c>, <c>items</c>, <c>pattern</c>,
/// <c>minLength</c>, <c>oneOf</c> and <c>anyOf</c>. It exists so the divergence test can
/// judge sample payloads against the schema the server owns, without a schema package.
/// </summary>
internal static class JsonSchemaLite
{
    /// <summary>Validates <paramref name="instance"/> against <paramref name="schema"/>.</summary>
    public static IReadOnlyList<string> Validate(JsonElement schema, JsonElement instance) =>
        Validate(schema, schema, instance);

    /// <summary>Validates against a sub-schema, resolving its <c>$ref</c>s against
    /// <paramref name="rootSchema"/> (the whole document).</summary>
    public static IReadOnlyList<string> Validate(JsonElement rootSchema, JsonElement schema, JsonElement instance)
    {
        var errors = new List<string>();
        ValidateNode(rootSchema, schema, instance, "$", errors);
        return errors;
    }

    private static void ValidateNode(JsonElement root, JsonElement schema, JsonElement instance, string path, List<string> errors)
    {
        if (schema.ValueKind != JsonValueKind.Object)
        {
            return;
        }

        if (schema.TryGetProperty("$ref", out JsonElement reference))
        {
            string? target = reference.GetString();
            if (target is null || !target.StartsWith("#/$defs/", StringComparison.Ordinal))
            {
                errors.Add(string.Create(CultureInfo.InvariantCulture, $"{path}: unsupported $ref {target}"));
                return;
            }

            if (!root.TryGetProperty("$defs", out JsonElement defs)
                || !defs.TryGetProperty(target["#/$defs/".Length..], out JsonElement definition))
            {
                errors.Add(string.Create(CultureInfo.InvariantCulture, $"{path}: $ref {target} does not resolve"));
                return;
            }

            ValidateNode(root, definition, instance, path, errors);
            return;
        }

        if (schema.TryGetProperty("type", out JsonElement type))
        {
            string expected = type.GetString() ?? string.Empty;
            if (!HasType(instance, expected))
            {
                errors.Add(string.Create(
                    CultureInfo.InvariantCulture,
                    $"{path}: expected {expected}, found {instance.ValueKind}"));
                return;
            }
        }

        if (schema.TryGetProperty("const", out JsonElement constant) && !DeepEquals(constant, instance))
        {
            errors.Add(string.Create(
                CultureInfo.InvariantCulture, $"{path}: expected the constant {constant.GetRawText()}"));
            return;
        }

        if (schema.TryGetProperty("enum", out JsonElement allowed) && allowed.ValueKind == JsonValueKind.Array)
        {
            bool found = allowed.EnumerateArray().Any(item => DeepEquals(item, instance));
            if (!found)
            {
                errors.Add(string.Create(CultureInfo.InvariantCulture, $"{path}: not one of the enum's values"));
                return;
            }
        }

        if (schema.TryGetProperty("oneOf", out JsonElement oneOf) && oneOf.ValueKind == JsonValueKind.Array)
        {
            int matches = 0;
            foreach (JsonElement option in oneOf.EnumerateArray())
            {
                var optionErrors = new List<string>();
                ValidateNode(root, option, instance, path, optionErrors);
                if (optionErrors.Count == 0)
                {
                    matches++;
                }
            }

            if (matches != 1)
            {
                errors.Add(string.Create(
                    CultureInfo.InvariantCulture, $"{path}: matched {matches} oneOf branches, exactly one must match"));
                return;
            }
        }

        if (schema.TryGetProperty("anyOf", out JsonElement anyOf) && anyOf.ValueKind == JsonValueKind.Array)
        {
            bool any = false;
            foreach (JsonElement option in anyOf.EnumerateArray())
            {
                var optionErrors = new List<string>();
                ValidateNode(root, option, instance, path, optionErrors);
                any |= optionErrors.Count == 0;
            }

            if (!any)
            {
                errors.Add(string.Create(CultureInfo.InvariantCulture, $"{path}: matched no anyOf branch"));
                return;
            }
        }

        if (instance.ValueKind == JsonValueKind.String)
        {
            string text = instance.GetString() ?? string.Empty;
            if (schema.TryGetProperty("minLength", out JsonElement minLength)
                && text.Length < minLength.GetInt32())
            {
                errors.Add(string.Create(
                    CultureInfo.InvariantCulture, $"{path}: shorter than minLength {minLength.GetInt32()}"));
            }

            if (schema.TryGetProperty("pattern", out JsonElement pattern)
                && !Regex.IsMatch(text, pattern.GetString() ?? string.Empty))
            {
                errors.Add(string.Create(
                    CultureInfo.InvariantCulture, $"{path}: does not match the pattern {pattern.GetString()}"));
            }
        }

        if (instance.ValueKind == JsonValueKind.Object)
        {
            JsonElement properties = schema.TryGetProperty("properties", out JsonElement declared) ? declared : default;
            if (schema.TryGetProperty("required", out JsonElement required) && required.ValueKind == JsonValueKind.Array)
            {
                foreach (JsonElement name in required.EnumerateArray())
                {
                    string member = name.GetString() ?? string.Empty;
                    if (!instance.TryGetProperty(member, out _))
                    {
                        errors.Add(string.Create(CultureInfo.InvariantCulture, $"{path}: is missing `{member}`"));
                    }
                }
            }

            bool closed = schema.TryGetProperty("additionalProperties", out JsonElement additional)
                && additional.ValueKind == JsonValueKind.False;
            foreach (JsonProperty member in instance.EnumerateObject())
            {
                if (properties.ValueKind == JsonValueKind.Object && properties.TryGetProperty(member.Name, out JsonElement memberSchema))
                {
                    ValidateNode(
                        root,
                        memberSchema,
                        member.Value,
                        string.Create(CultureInfo.InvariantCulture, $"{path}.{member.Name}"),
                        errors);
                    continue;
                }

                if (closed)
                {
                    errors.Add(string.Create(
                        CultureInfo.InvariantCulture, $"{path}: `{member.Name}` is not a declared member"));
                }
            }
        }

        if (instance.ValueKind == JsonValueKind.Array && schema.TryGetProperty("items", out JsonElement items))
        {
            int index = 0;
            foreach (JsonElement item in instance.EnumerateArray())
            {
                ValidateNode(root, items, item, string.Create(CultureInfo.InvariantCulture, $"{path}[{index}]"), errors);
                index++;
            }
        }
    }

    private static bool HasType(JsonElement instance, string expected) => expected switch
    {
        "object" => instance.ValueKind == JsonValueKind.Object,
        "array" => instance.ValueKind == JsonValueKind.Array,
        "string" => instance.ValueKind == JsonValueKind.String,
        "boolean" => instance.ValueKind is JsonValueKind.True or JsonValueKind.False,
        "integer" => instance.ValueKind == JsonValueKind.Number && instance.TryGetInt64(out _),
        "number" => instance.ValueKind == JsonValueKind.Number,
        "null" => instance.ValueKind == JsonValueKind.Null,
        _ => false,
    };

    internal static bool DeepEquals(JsonElement left, JsonElement right)
    {
        if (left.ValueKind != right.ValueKind)
        {
            return false;
        }

        return left.ValueKind switch
        {
            JsonValueKind.Object => left.EnumerateObject().Count() == right.EnumerateObject().Count()
                && left.EnumerateObject().All(property =>
                    right.TryGetProperty(property.Name, out JsonElement other) && DeepEquals(property.Value, other)),
            JsonValueKind.Array => left.GetArrayLength() == right.GetArrayLength()
                && left.EnumerateArray().Zip(right.EnumerateArray()).All(pair => DeepEquals(pair.First, pair.Second)),
            JsonValueKind.String => string.Equals(left.GetString(), right.GetString(), StringComparison.Ordinal),
            JsonValueKind.Number => left.GetRawText() == right.GetRawText(),
            _ => true,
        };
    }
}
