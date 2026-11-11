using Spotledger.Client.Api.Generated.Models;

using System.Text.Json;

namespace Spotledger.Client.Metadata;

/// <summary>
/// One field of a DocType as the client runtime needs it (ADR-068 §6, the contract's
/// <c>FieldMeta</c> decoded): its storage name, label, kind, format, decimal scale and
/// precision, select options, link target, lookup spec (kept as data), the presentation
/// hints, required, default and permlevel. Renderer-neutral: no UI types.
/// </summary>
public sealed class FieldModel
{
    /// <summary>Creates the model from its parts (tests and the factory).</summary>
    public FieldModel(
        string name,
        string label,
        FieldKind kind,
        string? unknownKindName = null,
        string? format = null,
        int? scale = null,
        int? precision = null,
        IReadOnlyList<string>? options = null,
        string? linkTo = null,
        string? linkTypeField = null,
        string? childDoctype = null,
        string? linkField = null,
        int? maxChars = null,
        LookupData? lookup = null,
        FieldHintsModel? hints = null,
        bool required = false,
        bool readOnly = false,
        FieldValue? defaultValue = null,
        int permlevel = 0,
        FieldFormatParameters? formatParameters = null,
        bool multiple = false,
        string? dependsOn = null,
        string? mandatoryDependsOn = null,
        string? readOnlyDependsOn = null,
        string? defaultExpr = null)
    {
        Name = name;
        Label = label;
        Kind = kind;
        UnknownKindName = unknownKindName;
        Format = format;
        FormatParameters = formatParameters ?? new FieldFormatParameters();
        Multiple = kind == FieldKind.Select && multiple;
        Scale = scale;
        Precision = precision;
        Options = options ?? Array.Empty<string>();
        LinkTo = linkTo;
        LinkTypeField = linkTypeField;
        ChildDoctype = childDoctype;
        LinkField = linkField;
        MaxChars = maxChars;
        Lookup = lookup;
        Hints = hints ?? new FieldHintsModel();
        Required = required;
        ReadOnly = readOnly;
        Default = defaultValue;
        Permlevel = permlevel;
        DependsOn = NullIfEmpty(dependsOn);
        MandatoryDependsOn = NullIfEmpty(mandatoryDependsOn);
        ReadOnlyDependsOn = NullIfEmpty(readOnlyDependsOn);
        DefaultExpr = NullIfEmpty(defaultExpr);
    }

    private static string? NullIfEmpty(string? text) => string.IsNullOrEmpty(text) ? null : text;

    /// <summary>The field's storage name (the identifier every API calls carry).</summary>
    public string Name { get; }

    /// <summary>The field's human label.</summary>
    public string Label { get; }

    /// <summary>The field's kind.</summary>
    public FieldKind Kind { get; }

    /// <summary>The encoding name when <see cref="Kind"/> is <see cref="FieldKind.Unknown"/>
    /// (what a newer server calls it); null otherwise.</summary>
    public string? UnknownKindName { get; }

    /// <summary>How the value is interpreted and shown (ADR-068 §2): <c>email</c>,
    /// <c>currency</c> and so on; null means plain. Unknown formats are treated as
    /// plain by renderers, kept verbatim here.</summary>
    public string? Format { get; }

    /// <summary>The decoded, typed format parameters (never caller-specific presentation).</summary>
    public FieldFormatParameters FormatParameters { get; }

    /// <summary>True for a select holding several options rather than a single text.</summary>
    public bool Multiple { get; }

    /// <summary>The digits after the point, for a decimal.</summary>
    public int? Scale { get; }

    /// <summary>The total digits, for a decimal (1 to 38 in the contract).</summary>
    public int? Precision { get; }

    /// <summary>The allowed values of a select field, in display order.</summary>
    public IReadOnlyList<string> Options { get; }

    /// <summary>The target DocType of a link.</summary>
    public string? LinkTo { get; }

    /// <summary>For a dynamic link, the field holding the target DocType.</summary>
    public string? LinkTypeField { get; }

    /// <summary>For a child table, the row DocType's storage name.</summary>
    public string? ChildDoctype { get; }

    /// <summary>For a table_multiselect, the row field holding the picked document.</summary>
    public string? LinkField { get; }

    /// <summary>The character limit of a text field.</summary>
    public int? MaxChars { get; }

    /// <summary>The field's base lookup (ADR-068 §3.2), kept as data. The client never
    /// sends it back; the server resolves which seam answers.</summary>
    public LookupData? Lookup { get; }

    /// <summary>The presentation hints (they never change what is stored).</summary>
    public FieldHintsModel Hints { get; }

    /// <summary>True when a value must be present for the document to validate.</summary>
    public bool Required { get; }

    /// <summary>True when the user cannot edit the field.</summary>
    public bool ReadOnly { get; }

    /// <summary>The value a new document starts with, when the metadata declares one.</summary>
    public FieldValue? Default { get; }

    /// <summary>
    /// The rule expression that shows the field (the contract's <c>depends_on</c>,
    /// ADR-068 §1 + A3): the field is shown while it evaluates to <c>true</c>. Absent
    /// means always shown (subject to <see cref="FieldHintsModel.Hidden"/>). Source text
    /// in the shared expression language at the DocType's
    /// <see cref="DocTypeModel.ExprVersion"/>; <see cref="RuleEngine"/> parses it once per
    /// metadata version.
    /// </summary>
    public string? DependsOn { get; }

    /// <summary>The rule expression that makes the field required in addition to the
    /// static <see cref="Required"/> (<c>mandatory_depends_on</c>).</summary>
    public string? MandatoryDependsOn { get; }

    /// <summary>The rule expression that makes the field read-only in addition to the
    /// static <see cref="ReadOnly"/> (<c>read_only_depends_on</c>).</summary>
    public string? ReadOnlyDependsOn { get; }

    /// <summary>The expression a new document or child row starts the field with
    /// (<c>default_expr</c>), used instead of <see cref="Default"/>; evaluated exactly
    /// once, on creation.</summary>
    public string? DefaultExpr { get; }

    /// <summary>True when the field carries at least one rule or default expression.</summary>
    public bool HasExpressions =>
        DependsOn is not null || MandatoryDependsOn is not null || ReadOnlyDependsOn is not null || DefaultExpr is not null;

    /// <summary>The numeric permission level the field arrived with (ADR-068 §1.3; the
    /// A1 amendment proposes replacing it with protection groups — kept until then).</summary>
    public int Permlevel { get; }

    /// <summary>True when the field renders in a grid's default columns.</summary>
    public bool InListView => Hints.InListView;

    /// <summary>True when the field takes part in the standard filter.</summary>
    public bool InStandardFilter => Hints.InStandardFilter;

    /// <summary>
    /// Parses typed text into a value for this field's kind with culture-invariant
    /// rules: decimals at the field's scale (more places is an error, never rounded),
    /// thousands separators only when the settings allow them, and so on. A null or
    /// empty text parses to null. Answers false with <paramref name="error"/> set when
    /// the text is not a value of the kind.
    /// </summary>
    public bool TryParse(
        string? text,
        out FieldValue? value,
        [System.Diagnostics.CodeAnalysis.NotNullWhen(false)] out FieldError? error,
        NumberParsingSettings? settings = null)
    {
        value = null;
        error = null;
        var parsing = settings ?? NumberParsingSettings.Strict;
        if (text is null || text.Length == 0)
        {
            value = FieldValue.Null;
            return true;
        }

        switch (Kind)
        {
            case FieldKind.DecimalNumber:
                return TryParseDecimal(text, parsing, out value, out error);
            case FieldKind.IntegerNumber:
                return TryParseInt(text, parsing, out value, out error);
            case FieldKind.Text:
            case FieldKind.LongText:
                value = new TextFieldValue(text);
                return true;
            case FieldKind.Select:
                return TryParseSelect(text, out value, out error);
            case FieldKind.Date:
                return TryParseDate(text, out value, out error);
            case FieldKind.Time:
                return TryParseTime(text, out value, out error);
            case FieldKind.Datetime:
                return TryParseDateTime(text, out value, out error);
            case FieldKind.Bool:
                return TryParseBool(text, out value, out error);
            case FieldKind.Link:
            case FieldKind.DynamicLink:
                value = new LinkFieldValue(text, null);
                return true;
            case FieldKind.Attach:
                value = new AttachFieldValue(text);
                return true;
            case FieldKind.Json:
                return TryParseJson(text, out value, out error);
            case FieldKind.Bytes:
                return TryParseBytes(text, out value, out error);
            case FieldKind.Secret:
                // A secret's value never passes through the draft; the renderer asks
                // the secrets service to set one. Parsing text is meaningless.
                error = new FieldError(Name, "field.secret_not_editable", "A secret is set, not typed here.");
                return false;
            case FieldKind.Table:
                error = new FieldError(Name, "field.table_not_text", "A child table is edited as rows, not as text.");
                return false;
            case FieldKind.Unknown:
                // A newer server's kind is shown but never edited: writing its value
                // back as plain text could corrupt the server's encoding.
                error = new FieldError(
                    Name,
                    "field.unknown_kind",
                    "This field's kind is newer than this client; it cannot be edited.");
                return false;
            default:
                error = new FieldError(Name, "field.unknown_kind", $"The field kind {Kind} is not editable as text.");
                return false;
        }
    }

    private bool TryParseDecimal(string text, NumberParsingSettings parsing, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        var scale = Scale ?? 0;
        var precision = Precision ?? 38;
        if (!DecimalFormat.TryParseUser(text, parsing.AllowThousandsSeparators, out var canonical))
        {
            error = new FieldError(Name, "field.invalid_decimal", "Type a number, like 1250 or 1250.00.");
            return false;
        }

        if (!DecimalFormat.TryParseExact(canonical, out var digits, out var fraction, out var _))
        {
            error = new FieldError(Name, "field.invalid_decimal", "Type a number, like 1250 or 1250.00.");
            return false;
        }

        if (fraction.Length > scale)
        {
            error = new FieldError(
                Name,
                "field.scale_exceeded",
                $"At most {scale} decimal place{(scale == 1 ? string.Empty : "s")} (no rounding).");
            return false;
        }

        var totalDigits = digits.Length + fraction.Length;
        if (totalDigits > precision)
        {
            error = new FieldError(
                Name,
                "field.precision_exceeded",
                $"At most {precision} digits in total.");
            return false;
        }

        if (!ExactDecimal.TryCreate(canonical, precision, out var exact))
        {
            error = new FieldError(Name, "field.invalid_decimal", "Type a number, like 1250 or 1250.00.");
            return false;
        }

        value = new DecimalFieldValue(exact);
        return true;
    }

    private bool TryParseInt(string text, NumberParsingSettings parsing, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        if (!DecimalFormat.TryParseUser(text, parsing.AllowThousandsSeparators, out var canonical)
            || !DecimalFormat.TryParseExact(canonical, out var digits, out var fraction, out var negative)
            || fraction.Length > 0)
        {
            error = new FieldError(Name, "field.invalid_int", "Type a whole number, like 42.");
            return false;
        }

        var digitCount = digits.Length == 1 && digits[0] == '0' ? 0 : digits.Length;
        if (digitCount > 19
            || (digitCount == 19
                && string.CompareOrdinal(negative ? "9223372036854775808" : "9223372036854775807", digits) < 0))
        {
            error = new FieldError(Name, "field.int_out_of_range", "The number is too large.");
            return false;
        }

        if (!long.TryParse((negative ? "-" : string.Empty) + digits, out var parsed))
        {
            error = new FieldError(Name, "field.int_out_of_range", "The number is too large.");
            return false;
        }

        value = new IntFieldValue(parsed);
        return true;
    }

    private bool TryParseSelect(string text, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        if (Multiple)
        {
            try
            {
                using var document = JsonDocument.Parse(text);
                if (document.RootElement.ValueKind == JsonValueKind.Array
                    && document.RootElement.EnumerateArray().All(item => item.ValueKind == JsonValueKind.String
                        && Options.Contains(item.GetString()!, StringComparer.Ordinal)))
                {
                    var selected = document.RootElement.EnumerateArray().Select(item => item.GetString()!).ToHashSet(StringComparer.Ordinal);
                    value = new MultiSelectFieldValue(Array.AsReadOnly(Options.Where(selected.Contains).ToArray()));
                    return true;
                }
            }
            catch (JsonException) { }
            error = new FieldError(Name, "field.not_an_option", "Choose an array of listed options.");
            return false;
        }
        if (!Options.Contains(text, StringComparer.Ordinal))
        {
            error = new FieldError(Name, "field.not_an_option", $"Choose one of: {string.Join(", ", Options)}.");
            return false;
        }

        value = new SelectFieldValue(text);
        return true;
    }

    private bool TryParseDate(string text, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        if (!TemporalFormat.TryParseDate(text, out var date))
        {
            error = new FieldError(Name, "field.invalid_date", "Type a date as YYYY-MM-DD.");
            return false;
        }

        value = new DateFieldValue(date);
        return true;
    }

    private bool TryParseTime(string text, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        if (!TemporalFormat.TryParseTime(text, out var time))
        {
            error = new FieldError(Name, "field.invalid_time", "Type a time as HH:MM:SS.");
            return false;
        }

        value = new TimeFieldValue(time);
        return true;
    }

    private bool TryParseDateTime(string text, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        if (!TemporalFormat.TryParseDateTime(text, out var moment))
        {
            error = new FieldError(Name, "field.invalid_datetime", "Type a date and time, like 2026-09-29T14:30:00Z.");
            return false;
        }

        value = new DateTimeFieldValue(moment);
        return true;
    }

    private bool TryParseBool(string text, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        if (text is "true" or "false")
        {
            value = new BoolFieldValue(text == "true");
            return true;
        }

        error = new FieldError(Name, "field.invalid_bool", "Type true or false.");
        return false;
    }

    private bool TryParseJson(string text, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        try
        {
            using var _ = JsonDocument.Parse(text);
        }
        catch (JsonException)
        {
            error = new FieldError(Name, "field.invalid_json", "Type valid JSON.");
            return false;
        }

        value = new JsonFieldValue(text);
        return true;
    }

    private bool TryParseBytes(string text, out FieldValue? value, out FieldError? error)
    {
        value = null;
        error = null;
        if (!IsBase64(text))
        {
            error = new FieldError(Name, "field.invalid_base64", "Type standard base64.");
            return false;
        }

        value = new BytesFieldValue(text);
        return true;
    }

    private static bool IsBase64(string text)
    {
        if (text.Length == 0 || text.Length % 4 != 0)
        {
            return false;
        }

        foreach (var c in text)
        {
            var ok = (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9')
                || c == '+' || c == '/' || c == '=' || c == '-';
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }
}

/// <summary>The presentation hints of a field (ADR-068 §1.3); they never change what is
/// stored.</summary>
/// <param name="Description">Longer offline help text.</param>
/// <param name="Hidden">The UI hides it (hiding is not security).</param>
/// <param name="InListView">Shown in a list's default columns.</param>
/// <param name="InStandardFilter">Offered in the standard filter.</param>
/// <param name="NoCopy">Not carried by a duplicate.</param>
/// <param name="Permlevel">The numeric permission level (deprecated by ADR-068 A1;
/// kept as data until the amendment lands).</param>
/// <param name="Control">The known control preference, or Unknown to use the renderer default.</param>
/// <param name="RawControl">The original wire text, including unrecognized additive hints.</param>
public sealed record FieldHintsModel(
    string? Description = null,
    bool Hidden = false,
    bool InListView = false,
    bool InStandardFilter = false,
    bool NoCopy = false,
    int Permlevel = 0,
    ControlHint Control = ControlHint.Unknown,
    string? RawControl = null);

/// <summary>
/// The field's base lookup spec (ADR-068 §3.2), kept as data: the seam name (absent
/// means the platform default <c>platform:link_search</c>), its protocol major, and the
/// fixed parameters. Never sent back to the server.
/// </summary>
/// <param name="Seam">The seam name, or null for the platform default.</param>
/// <param name="Version">The lookup protocol major.</param>
/// <param name="Params">The fixed parameters.</param>
public sealed record LookupData(string? Seam, int? Version, IReadOnlyDictionary<string, string>? Params)
{
    /// <summary>The effective seam name: the declared one or the platform default.</summary>
    public string EffectiveSeam => Seam ?? "platform:link_search";
}