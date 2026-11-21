using System.Globalization;
using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Metadata;

/// <summary>
/// A DocType as the client runtime needs it: the name, label, version (the ETag-backed
/// metadata version), the posted flag, the fields in display order, and one child
/// table model per table field. Built from the generated <c>DocTypeMeta</c> by
/// <see cref="MetaModelFactory"/>; renderer-neutral.
/// </summary>
public sealed class DocTypeModel
{
    /// <summary>Creates the model from its parts (tests and the factory).</summary>
    public DocTypeModel(
        string name,
        string label,
        int version,
        bool isPosted,
        IReadOnlyList<FieldModel> fields,
        IReadOnlyDictionary<string, DocTypeModel>? childTables = null,
        int? exprVersion = null)
    {
        Name = name;
        Label = label;
        Version = version;
        IsPosted = isPosted;
        Fields = fields;
        FieldsByName = fields.ToDictionary(f => f.Name, StringComparer.Ordinal);
        ChildTables = childTables ?? EmptyChildTables;
        ExprVersion = exprVersion;
    }

    private static readonly IReadOnlyDictionary<string, DocTypeModel> EmptyChildTables =
        new Dictionary<string, DocTypeModel>();

    /// <summary>The DocType's storage name.</summary>
    public string Name { get; }

    /// <summary>The DocType's human label.</summary>
    public string Label { get; }

    /// <summary>The metadata version (the ETag's number; 1 or more).</summary>
    public int Version { get; }

    /// <summary>True when documents of this DocType have a draft stage and a Post
    /// action (ADR-42).</summary>
    public bool IsPosted { get; }

    /// <summary>
    /// The shared expression language version the field rule expressions are written for
    /// (<c>expr_version</c>, WP-CON-03.T7 / ADR-068 A3); null when the DocType carries no
    /// expressions. A client that does not implement the version ignores every rule
    /// expression and uses the static properties — <see cref="RuleEngine"/> does exactly
    /// that and reports it as a diagnostic.
    /// </summary>
    public int? ExprVersion { get; }

    /// <summary>The fields in definition (display) order.</summary>
    public IReadOnlyList<FieldModel> Fields { get; }

    /// <summary>The fields by storage name.</summary>
    public IReadOnlyDictionary<string, FieldModel> FieldsByName { get; }

    /// <summary>The child table field name to that table's row DocType model.</summary>
    public IReadOnlyDictionary<string, DocTypeModel> ChildTables { get; }

    /// <summary>Looks a field up by name; null when the DocType has no such field.</summary>
    public FieldModel? Field(string name) => FieldsByName.TryGetValue(name, out var field) ? field : null;

    /// <summary>The row model of a child table field; null when the field is not a table.</summary>
    public DocTypeModel? ChildTable(string name) => ChildTables.TryGetValue(name, out var row) ? row : null;
}

/// <summary>
/// One field of a child table row as the metadata runtime needs it — the same shape as
/// <see cref="FieldModel"/> but with <c>table</c>-kinds excluded (a child table has no
/// grandchild tables: the contract's <c>ChildRow</c> carries scalar values only).
/// The row model's fields are the same <see cref="FieldModel"/> instances.
/// </summary>
/// <param name="Model">The row DocType model.</param>
/// <param name="LinkField">For a <c>table_multiselect</c>, the row field holding the
/// picked document's id.</param>
public sealed record TableRowModel(DocTypeModel Model, string? LinkField);

/// <summary>
/// Builds <see cref="DocTypeModel"/> and <see cref="FieldModel"/> from the generated
/// contract models. A newer server's additive additions (an unknown field-type name, an
/// unknown hint) degrade to data, never to a crash: unknown kinds become
/// <see cref="FieldKind.Unknown"/> with the name kept.
/// </summary>
public static class MetaModelFactory
{
    /// <summary>
    /// Builds the model from the generated metadata body and the ETag it arrived with.
    /// </summary>
    /// <param name="meta">The generated <c>DocTypeMeta</c>.</param>
    /// <param name="eTag">The metadata ETag (for example <c>"7"</c>); the version falls
    /// back to the body's <c>version</c> when the header did not arrive.</param>
    public static DocTypeModel Create(DocTypeMeta meta, string? eTag = null)
    {
        ArgumentNullException.ThrowIfNull(meta);
        if (meta.Doctype is null)
        {
            throw new ArgumentException("The metadata body carries no doctype name.", nameof(meta));
        }

        var version = eTag is { Length: >= 3 } tag && tag[0] == '"' && tag[^1] == '"'
            && int.TryParse(tag[1..^1], out var fromTag)
            ? fromTag
            : meta.Version ?? 0;

        var fields = new List<FieldModel>();
        var children = new Dictionary<string, DocTypeModel>(StringComparer.Ordinal);

        var contractFields = meta.Fields ?? new List<FieldMeta>();
        foreach (var field in contractFields)
        {
            if (field?.Name is null)
            {
                continue;
            }

            var model = CreateField(field);
            fields.Add(model);

            if (model.Kind == FieldKind.Table)
            {
                children[model.Name] = CreateChildRowModel(field);
            }
        }

        return new DocTypeModel(
            meta.Doctype,
            meta.Label ?? meta.Doctype,
            version,
            meta.IsPosted ?? false,
            fields,
            children,
            meta.ExprVersion);
    }

    private static FieldModel CreateField(FieldMeta field)
    {
        // Keep the generated enum's name for a kind the client predates (an unknown
        // encoding): the name is the only thing an upgrade can map back.
        var encodingName = field.Type is { } t ? EncodingName(t) : null;
        var kind = encodingName is null ? FieldKind.Unknown : FieldKindExtensions.FromEncodingName(encodingName);
        if (field.LinkTypeField is { Length: > 0 } && kind == FieldKind.Link)
        {
            kind = FieldKind.DynamicLink;
        }

        if (kind == FieldKind.Text && field.Select is not null)
        {
            kind = FieldKind.Select;
        }

        var unknownKindName = kind == FieldKind.Unknown
            ? (field.Type is { } declaredType ? EnumNameOrDefault(declaredType) ?? declaredType.ToString() : null)
            : null;
        var name = field.Name ?? string.Empty;
        var label = field.Label ?? name;

        return new FieldModel(
            name: name,
            label: label,
            kind: kind,
            unknownKindName: unknownKindName,
            format: NullIfEmpty(field.Format),
            scale: field.Scale,
            precision: field.Precision,
            options: field.Select is { Count: > 0 } select ? select : null,
            linkTo: NullIfEmpty(field.LinkTo),
            linkTypeField: NullIfEmpty(field.LinkTypeField),
            childDoctype: NullIfEmpty(field.ChildDoctype),
            linkField: NullIfEmpty(field.LinkField),
            maxChars: field.MaxChars,
            lookup: field.Lookup is null ? null : CreateLookup(field.Lookup),
            hints: CreateHints(field.Hints),
            required: field.Required ?? false,
            readOnly: field.ReadOnly ?? false,
            defaultValue: field is PreservedFieldMeta { MultipleDefault: { } selected } && field.Multiple == true
                ? new MultiSelectFieldValue(selected)
                : field.Default is null ? null : ValueCodec.DecodeDefault(field.Default, kind),
            formatParameters: FieldFormatParameters.Decode(field.FormatParams?.AdditionalData),
            multiple: field.Multiple ?? false,
            dependsOn: field.DependsOn,
            mandatoryDependsOn: field.MandatoryDependsOn,
            readOnlyDependsOn: field.ReadOnlyDependsOn,
            defaultExpr: field.DefaultExpr);
    }

    private static FieldHintsModel CreateHints(FieldHints? hints)
    {
        if (hints is null)
        {
            return new FieldHintsModel();
        }

        // read_only and required are field-level properties, not hints; they are
        // carried by the FieldModel itself.
        return new FieldHintsModel(
            Description: hints.Description,
            Hidden: hints.Hidden ?? false,
            InListView: hints.InListView ?? false,
            InStandardFilter: hints.InStandardFilter ?? false,
            NoCopy: hints.NoCopy ?? false,
            Permlevel: hints.Permlevel ?? 0,
            Control: DecodeControl(hints),
            RawControl: RawControl(hints));
    }

    private static string? RawControl(FieldHints hints) => hints is PreservedFieldHints preserved
        ? preserved.RawControl : hints.Control?.ToString().ToLowerInvariant();

    private static ControlHint DecodeControl(FieldHints hints) => RawControl(hints) switch
    {
        "switch" => ControlHint.Switch,
        "checkbox" => ControlHint.Checkbox,
        "dropdown" => ControlHint.Dropdown,
        "radio" => ControlHint.Radio,
        "segmented" => ControlHint.Segmented,
        "autocomplete" => ControlHint.Autocomplete,
        "chips" => ControlHint.Chips,
        "textarea" => ControlHint.Textarea,
        _ => ControlHint.Unknown,
    };

    private static LookupData CreateLookup(LookupSpec lookup) => new(
        Seam: lookup.Seam,
        Version: lookup.Version,
        Params: lookup.Params is null
            ? null
            : new Dictionary<string, string>(
                lookup.Params.AdditionalData?.Select(pair => new KeyValuePair<string, string>(
                    pair.Key,
                    pair.Value switch
                    {
                        string s => s,
                        int i => i.ToString(CultureInfoDefaults),
                        bool b => b ? "true" : "false",
                        _ => pair.Value?.ToString() ?? string.Empty,
                    })) ?? Enumerable.Empty<KeyValuePair<string, string>>(),
                StringComparer.Ordinal));

    private static readonly CultureInfo CultureInfoDefaults = CultureInfo.InvariantCulture;

    private static DocTypeModel CreateChildRowModel(FieldMeta tableField)
    {
        // The contract's child_doctype names the row DocType; the metadata for the row
        // DocType arrives through meta_get of that DocType (WP-MET-01). Until it is
        // fetched, the row model is empty: rows carry their raw fields.
        var name = tableField.ChildDoctype ?? tableField.Name ?? string.Empty;
        return new DocTypeModel(
            name,
            tableField.Label ?? name,
            0,
            isPosted: false,
            fields: Array.Empty<FieldModel>());
    }

    private static string? NullIfEmpty(string? text) => string.IsNullOrEmpty(text) ? null : text;

    private static string? EncodingName(FieldType type) => type switch
    {
        FieldType.Bool => "bool",
        FieldType.Int => "int",
        FieldType.Decimal => "decimal",
        FieldType.Text => "text",
        FieldType.Long_text => "long_text",
        FieldType.Date => "date",
        FieldType.Time => "time",
        FieldType.Datetime => "datetime",
        FieldType.Link => "link",
        FieldType.Json => "json",
        FieldType.Bytes => "bytes",
        FieldType.Attach => "attach",
        FieldType.Secret => "secret",
        FieldType.Table => "table",
        FieldType.Table_multiselect => "table_multiselect",
        _ => null,
    };

    private static string? EnumNameOrDefault(FieldType? type) => type is null ? null : type switch
    {
        FieldType.Bool => "bool",
        FieldType.Int => "int",
        FieldType.Decimal => "decimal",
        FieldType.Text => "text",
        FieldType.Long_text => "long_text",
        FieldType.Date => "date",
        FieldType.Time => "time",
        FieldType.Datetime => "datetime",
        FieldType.Link => "link",
        FieldType.Json => "json",
        FieldType.Bytes => "bytes",
        FieldType.Attach => "attach",
        FieldType.Secret => "secret",
        FieldType.Table => "table",
        FieldType.Table_multiselect => "table_multiselect",
        _ => null,
    };
}