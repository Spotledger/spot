namespace Spotledger.Client.Effects;

/// <summary>The JSON shape of one member of an effect object, as the schema declares it.</summary>
/// <param name="Name">The member's wire name.</param>
/// <param name="Type">The JSON type the schema declares (<c>string</c>, <c>boolean</c>,
/// <c>array</c>, <c>object</c>), or <c>value</c> for the untyped <c>set_value.value</c>,
/// <c>path</c> for a field path, <c>ref:option</c>/<c>ref:effect</c>/<c>ref:message_level</c>
/// for the schema's <c>$ref</c> members, and <c>ref:object</c> for an <c>anyOf</c>-only
/// requirement group.</param>
/// <param name="Required">Whether the schema lists the member in <c>required</c>.</param>
public sealed record EffectMemberShape(string Name, string Type, bool Required);

/// <summary>One effect kind as the schema declares it.</summary>
/// <param name="Kind">The <c>kind</c> const.</param>
/// <param name="Members">The object's members (the <c>kind</c> member included).</param>
/// <param name="AtLeastOneOf">Members of which at least one must be present, when the schema
/// declares an <c>anyOf</c> requirement group (only <c>show_message</c> does).</param>
public sealed record EffectKindShape(string Kind, IReadOnlyList<EffectMemberShape> Members, IReadOnlyList<string>? AtLeastOneOf = null)
{
    /// <summary>The members the schema requires.</summary>
    public IReadOnlyList<string> RequiredMemberNames
    {
        get
        {
            var names = new List<string>();
            foreach (EffectMemberShape member in Members)
            {
                if (member.Required)
                {
                    names.Add(member.Name);
                }
            }

            return names;
        }
    }
}

/// <summary>
/// The closed set of v1 effect kinds as data: the single list the wire reader and the
/// schema-divergence test share, so a C# model that drifts from
/// <c>schema/effects.v1.json</c> fails a test instead of failing in the field.
/// </summary>
public static class EffectSchema
{
    private const string Path = "path";
    private const string RefEffect = "ref:effect";
    private const string RefOption = "ref:option";
    private const string RefLevel = "ref:message_level";

    /// <summary>Every kind of v1, with the members its schema declares.</summary>
    public static IReadOnlyList<EffectKindShape> Kinds { get; } = Array.AsReadOnly(new[]
    {
        new EffectKindShape("set_value", new[]
        {
            Required("kind", "string"), Required("path", Path), Required("value", "value"),
        }),
        new EffectKindShape("set_read_only", new[]
        {
            Required("kind", "string"), Required("path", Path), Required("read_only", "boolean"),
        }),
        new EffectKindShape("set_hidden", new[]
        {
            Required("kind", "string"), Required("path", Path), Required("hidden", "boolean"),
        }),
        new EffectKindShape("set_required", new[]
        {
            Required("kind", "string"), Required("path", Path), Required("required", "boolean"),
        }),
        new EffectKindShape("set_options", new[]
        {
            Required("kind", "string"), Required("path", Path), Required("options", "ref:option_list"),
        }),
        new EffectKindShape("set_label", new[]
        {
            Required("kind", "string"), Required("path", Path), Required("label", "string"),
        }),
        new EffectKindShape("add_action", new[]
        {
            Required("kind", "string"), Required("id", "string"), Required("label", "string"),
            Required("effect", RefEffect),
        }),
        new EffectKindShape("remove_action", new[]
        {
            Required("kind", "string"), Required("id", "string"),
        }),
        new EffectKindShape("show_message", new[]
        {
            Required("kind", "string"), Required("level", RefLevel), Optional("text", "string"),
            Optional("text_key", "string"), Optional("field", Path),
        }, new[] { "text", "text_key" }),
        new EffectKindShape("open_dialog", new[]
        {
            Required("kind", "string"), Required("dialog_id", "string"), Optional("params", "object"),
        }),
        new EffectKindShape("focus", new[]
        {
            Required("kind", "string"), Required("path", Path),
        }),
        new EffectKindShape("set_filter", new[]
        {
            Required("kind", "string"), Required("link_path", Path), Required("filter", "string"),
        }),
        new EffectKindShape("batch", new[]
        {
            Required("kind", "string"), Required("effects", "ref:effect_list"),
        }),
    });

    /// <summary>The shape of one kind, or null when the kind is not v1's.</summary>
    public static EffectKindShape? For(string? kind)
    {
        foreach (EffectKindShape shape in Kinds)
        {
            if (string.Equals(shape.Kind, kind, StringComparison.Ordinal))
            {
                return shape;
            }
        }

        return null;
    }

    /// <summary>Whether the kind name is one v1 defines.</summary>
    public static bool IsKnownKind(string? kind) => For(kind) is not null;

    private static EffectMemberShape Required(string name, string type) => new(name, type, true);

    private static EffectMemberShape Optional(string name, string type) => new(name, type, false);
}
