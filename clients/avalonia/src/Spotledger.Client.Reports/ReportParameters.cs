using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Reports;

/// <summary>One parameter of the form: its declaration, the field model the generic field controls are built
/// from, and the state they commit into. Parsing and validation are the metadata runtime's, so a parameter
/// accepts exactly what a form field of the same kind accepts.</summary>
public sealed class ReportParameter
{
    /// <summary>The decimal places a decimal parameter accepts when its default shows no more (the contract carries no scale).</summary>
    public const int DefaultDecimalScale = 6;

    internal ReportParameter(ReportParameterDef def)
    {
        Def = def;
        Field = BuildField(def);
        var initial = ReadDefault(def, Field);
        State = new FieldState(initial, Field);
        Default = initial;
    }

    /// <summary>The declaration.</summary>
    public ReportParameterDef Def { get; }

    /// <summary>The field model the controls are built from.</summary>
    public FieldModel Field { get; }

    /// <summary>The state the field control commits into.</summary>
    public FieldState State { get; }

    /// <summary>The default (what Reset returns to).</summary>
    public FieldValue Default { get; }

    /// <summary>The parameter's name (the key the run request carries it under).</summary>
    public string Name => Def.Name;

    /// <summary>The current error message, or null.</summary>
    public string? Error => State.Errors.Count > 0 ? State.Errors[0].Message : null;

    /// <summary>The current value as the text the form shows (empty when no value).</summary>
    public string Text => TextOf(State.Value);

    /// <summary>True when no value is set.</summary>
    public bool IsEmpty => IsEmptyValue(State.Value);

    /// <summary>Parses typed text with the field's rules and sets the value; on a mistake the error is attached and false is returned.</summary>
    public bool TrySetText(string? text)
    {
        if (Field.TryParse(text?.Trim(), out var value, out var error, NumberParsingSettings.WithSeparators))
        {
            State.Value = value ?? FieldValue.Null;
            State.ClearErrors();
            return true;
        }

        State.SetErrors([error!]);
        return false;
    }

    /// <summary>Sets a value the form's control produced.</summary>
    public void SetValue(FieldValue value)
    {
        State.Value = value ?? FieldValue.Null;
        State.ClearErrors();
    }

    /// <summary>Returns to the default.</summary>
    public void Reset()
    {
        State.Value = Default;
        State.ClearErrors();
    }

    /// <summary>Attaches a message (a server answer naming this parameter).</summary>
    public void SetError(string code, string message) => State.SetErrors([new FieldError(Name, code, message)]);

    /// <summary>Checks the value: a control's parse error stays; a required parameter needs a value.</summary>
    public bool Validate()
    {
        if (State.Errors.Count > 0)
        {
            return false;
        }

        if (Def.Required && IsEmpty)
        {
            SetError("report.required", Def.Label + " is required.");
            return false;
        }

        return true;
    }

    /// <summary>Writes the value in the contract's encoding (the run request's filter value).</summary>
    internal void WriteTo(Utf8JsonWriter writer)
    {
        writer.WritePropertyName(Name);
        switch (State.Value)
        {
            case LinkFieldValue link:
                writer.WriteStringValue(link.Id);
                break;
            default:
                ValueCodec.WriteValue(writer, State.Value);
                break;
        }
    }

    internal static string TextOf(FieldValue value) => value switch
    {
        TextFieldValue text => text.Value,
        SelectFieldValue select => select.Value,
        IntFieldValue number => number.Value.ToString(CultureInfo.InvariantCulture),
        DecimalFieldValue number => number.CanonicalText,
        DateFieldValue date => date.Value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture),
        LinkFieldValue link => link.Id ?? string.Empty,
        BoolFieldValue flag => flag.Value ? "true" : "false",
        _ => string.Empty,
    };

    internal static bool IsEmptyValue(FieldValue value) => value switch
    {
        NullFieldValue => true,
        TextFieldValue text => text.Value.Length == 0,
        SelectFieldValue select => select.Value.Length == 0,
        LinkFieldValue link => string.IsNullOrEmpty(link.Id),
        _ => false,
    };

    private static FieldModel BuildField(ReportParameterDef def)
    {
        var kind = def.Type switch
        {
            ReportParameterType.IntegerNumber => FieldKind.IntegerNumber,
            ReportParameterType.DecimalNumber => FieldKind.DecimalNumber,
            ReportParameterType.Date => FieldKind.Date,
            ReportParameterType.Link => FieldKind.Link,
            ReportParameterType.Select => FieldKind.Select,
            ReportParameterType.Text => FieldKind.Text,
            _ => FieldKind.Unknown,
        };
        int? scale = null;
        if (kind == FieldKind.DecimalNumber)
        {
            var seen = def.Default is { ValueKind: JsonValueKind.String } d && d.GetString() is { } t && t.IndexOf('.', StringComparison.Ordinal) is var at and >= 0 ? t.Length - at - 1 : 0;
            scale = Math.Clamp(Math.Max(DefaultDecimalScale, seen), 0, 28);
        }

        return new FieldModel(
            def.Name,
            def.Label,
            kind,
            unknownKindName: kind == FieldKind.Unknown ? "report parameter" : null,
            scale: scale,
            precision: kind == FieldKind.DecimalNumber ? 38 : null,
            options: def.Options?.Values,
            linkTo: def.Options?.LinkDoctype,
            required: def.Required);
    }

    private static FieldValue ReadDefault(ReportParameterDef def, FieldModel field)
    {
        if (def.Default is not { } element || field.Kind == FieldKind.Unknown)
        {
            return FieldValue.Null;
        }

        try
        {
            return ValueCodec.DecodeElement(element, field);
        }
        catch (Exception error) when (error is FormatException or InvalidOperationException or ArgumentException or JsonException)
        {
            return FieldValue.Null;
        }
    }
}

/// <summary>The parameter form's model: the parameters in order, validation, the run request's filters, the
/// saved-set snapshot, the identity digest and the mapping of a server refusal onto the parameter it names.</summary>
public sealed class ReportParameters
{
    private readonly List<ReportParameter> _items;

    /// <summary>Builds the form model for a report's declared parameters.</summary>
    public ReportParameters(IEnumerable<ReportParameterDef> definitions)
    {
        ArgumentNullException.ThrowIfNull(definitions);
        _items = definitions.Select(d => new ReportParameter(d)).ToList();
    }

    /// <summary>The parameters in declaration order.</summary>
    public IReadOnlyList<ReportParameter> Items => _items;

    /// <summary>True when a parameter has a kind this client does not know: the report cannot be run here.</summary>
    public bool HasUnsupported => _items.Any(p => p.Def.Type == ReportParameterType.Unknown);

    /// <summary>The parameter named <paramref name="name"/>, or null.</summary>
    public ReportParameter? Find(string name) => _items.FirstOrDefault(p => string.Equals(p.Name, name, StringComparison.Ordinal));

    /// <summary>Returns every parameter to its default and clears errors.</summary>
    public void ResetAll()
    {
        foreach (var item in _items)
        {
            item.Reset();
        }
    }

    /// <summary>Validates every parameter (all errors are attached, not only the first). True when a run may go ahead.</summary>
    public bool Validate()
    {
        var valid = true;
        foreach (var item in _items)
        {
            valid &= item.Validate();
        }

        return valid;
    }

    /// <summary>The JSON object of the parameters that have a value, keyed by name.</summary>
    public string ToFiltersJson()
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            foreach (var item in _items.Where(p => !p.IsEmpty))
            {
                item.WriteTo(writer);
            }

            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>The values that are set, as text by name (what a saved set stores and a tab identity digests).</summary>
    public IReadOnlyDictionary<string, string> Snapshot()
        => _items.Where(p => !p.IsEmpty).ToDictionary(p => p.Name, p => p.Text, StringComparer.Ordinal);

    /// <summary>The digest of the current values (see <see cref="DigestOf"/>).</summary>
    public string Digest() => DigestOf(Snapshot());

    /// <summary>Applies a saved set or a tab's starting values: every parameter takes the value in
    /// <paramref name="values"/> or, when the set has none, its default. A value that no longer parses is left empty
    /// with its error attached. Returns the names that did not apply.</summary>
    public IReadOnlyList<string> Apply(IReadOnlyDictionary<string, string>? values)
    {
        var failed = new List<string>();
        foreach (var item in _items)
        {
            if (values is not null && values.TryGetValue(item.Name, out var text))
            {
                if (!item.TrySetText(text))
                {
                    failed.Add(item.Name);
                }
            }
            else
            {
                item.Reset();
            }
        }

        return failed;
    }

    /// <summary>Puts a server refusal (<c>report.bad_argument</c>) on the parameter it names: first the error's
    /// field list, then a parameter name in the message. Returns false when no parameter is named.</summary>
    public bool MapServerError(ApiError error)
    {
        ArgumentNullException.ThrowIfNull(error);
        foreach (var field in error.FieldErrors)
        {
            var name = field.Field.StartsWith("filters.", StringComparison.Ordinal) ? field.Field["filters.".Length..] : field.Field;
            if (Find(name) is { } parameter)
            {
                parameter.SetError(field.Code, field.Message);
                return true;
            }
        }

        foreach (var parameter in _items)
        {
            if (ContainsWord(error.Message, parameter.Name))
            {
                parameter.SetError(error.Code, error.Message);
                return true;
            }
        }

        return false;
    }

    /// <summary>The identity digest of a set of parameter values: SHA-256 over the sorted <c>name=value</c> lines,
    /// 16 hex digits. Two tabs of the same report with the same values have the same key.</summary>
    public static string DigestOf(IReadOnlyDictionary<string, string>? values)
    {
        var builder = new StringBuilder();
        foreach (var pair in (values ?? new Dictionary<string, string>()).Where(p => p.Value.Length > 0).OrderBy(p => p.Key, StringComparer.Ordinal))
        {
            builder.Append(pair.Key).Append('=').Append(pair.Value).Append('\n');
        }

        return Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(builder.ToString())))[..16].ToLowerInvariant();
    }

    private static bool ContainsWord(string text, string word)
    {
        var from = 0;
        while ((from = text.IndexOf(word, from, StringComparison.Ordinal)) >= 0)
        {
            var before = from == 0 || !IsNameChar(text[from - 1]);
            var end = from + word.Length;
            var after = end >= text.Length || !IsNameChar(text[end]);
            if (before && after)
            {
                return true;
            }

            from = end;
        }

        return false;
    }

    private static bool IsNameChar(char c) => char.IsLetterOrDigit(c) || c == '_';
}
