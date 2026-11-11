using System.Text.Json;

namespace Spotledger.Client.Metadata;

/// <summary>
/// Validates a <see cref="DocumentDraft"/> against its DocType model with
/// culture-invariant rules: required fields present, text within its character limit,
/// select values among the options, decimals at the field's scale. Answers every
/// violation as a <see cref="FieldError"/> carrying the field path the server's field
/// errors use (<c>items[3].qty</c>, rows counted from 1).
/// </summary>
public static class DraftValidator
{
    /// <summary>Validates the whole draft (the document's fields and every table row).</summary>
    public static IReadOnlyList<FieldError> Validate(DocumentDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        var errors = new List<FieldError>();
        ValidateFields(draft, draft.Model, draft.Fields, null);
        foreach (var pair in draft.Tables)
        {
            var rowModel = pair.Value.RowModel;
            for (var i = 0; i < pair.Value.Rows.Count; i++)
            {
                ValidateRowFields(draft, rowModel, pair.Value.Rows[i].Values, TablePathExtensions.OfTable(pair.Key, i + 1), errors);
            }
        }

        return errors;

        void ValidateFields(
            DocumentDraft theDraft,
            DocTypeModel model,
            IReadOnlyDictionary<string, FieldState> states,
            string? rowPrefix)
        {
            foreach (var field in model.Fields)
            {
                if (!states.TryGetValue(field.Name, out var state))
                {
                    continue;
                }

                if (state.IsMasked) { continue; }
                var path = rowPrefix is null ? field.Name : $"{rowPrefix}.{field.Name}";
                ValidateOne(theDraft, field, state.Value, path, errors);
            }
        }

        void ValidateRowFields(
            DocumentDraft theDraft,
            DocTypeModel? rowModel,
            IReadOnlyDictionary<string, FieldState> states,
            string prefix,
            List<FieldError> into)
        {
            // A row's model may not be loaded yet (the row DocType's metadata arrives
            // separately); validate what the row's values claim by the types they carry.
            var fields = rowModel?.Fields ?? Array.Empty<FieldModel>();
            if (fields.Count == 0)
            {
                foreach (var pair in states)
                {
                    if (pair.Value.Value.IsNull)
                    {
                        continue;
                    }

                    if (pair.Value.Value is DecimalFieldValue decimalValue && pair.Value.Original is DecimalFieldValue)
                    {
                        // Without the row model the field's scale is unknown: nothing to
                        // check beyond shape, which parsing already did.
                        _ = decimalValue;
                    }
                }

                return;
            }

            foreach (var field in fields)
            {
                if (!states.TryGetValue(field.Name, out var state))
                {
                    if (field.Required)
                    {
                        into.Add(new FieldError($"{prefix}.{field.Name}", "document.required", $"{field.Label} is required."));
                    }

                    continue;
                }

                if (!state.IsMasked) { ValidateOne(theDraft, field, state.Value, $"{prefix}.{field.Name}", into); }
            }
        }
    }

    private static void ValidateOne(
        DocumentDraft draft,
        FieldModel field,
        FieldValue value,
        string path,
        List<FieldError> into)
    {
        // No value at all: null, the kinds' empty texts, a link with no id yet. A
        // required check fires against all of these.
        if (IsEmptyValue(value))
        {
            if (field.Required)
            {
                into.Add(new FieldError(path, "document.required", $"{field.Label} is required."));
            }

            return;
        }

        switch (value)
        {
            case MultiSelectFieldValue selected:
                if (!field.Multiple || !selected.Values.SequenceEqual(field.Options.Where(option => selected.Values.Contains(option, StringComparer.Ordinal)), StringComparer.Ordinal))
                {
                    into.Add(new FieldError(path, "document.not_an_option", $"{field.Label} must contain listed options once, in definition order."));
                }
                break;
            case TextFieldValue text:
                ValidateText(field, text.Value, path, into);
                break;
            case SelectFieldValue select:
                if (!field.Options.Contains(select.Value, StringComparer.Ordinal))
                {
                    into.Add(new FieldError(path, "document.not_an_option", $"{field.Label} must be one of the listed options."));
                }

                break;
            case DecimalFieldValue decimalValue:
                ValidateDecimal(field, decimalValue, path, into);
                break;
            case JsonFieldValue json:
                _ = json; // shape was checked at parse time
                break;
            case SecretFieldValue:
                // The secrets service holds the value; the draft can only carry the flag.
                break;
            default:
                break;
        }
    }

    private static void ValidateText(FieldModel field, string text, string path, List<FieldError> into)
    {
        if (field.MaxChars is { } limit && text.Length > limit)
        {
            into.Add(new FieldError(path, "document.max_length", $"{field.Label} is limited to {limit} characters."));
            return;
        }

        // The ADR-068 §2 formats are validated on save by the server; the client checks
        // only the plain shape it can know exactly (an unknown format is plain).
        if (field.Format is { } format)
        {
            var shapeError = format switch
            {
                "email" => IsEmailShape(text) ? null : new FieldError(path, "document.invalid_format", $"{field.Label} must be an e-mail address."),
                "url" => IsUrlShape(text) ? null : new FieldError(path, "document.invalid_format", $"{field.Label} must be a URL."),
                _ => null,
            };
            if (shapeError is { } error)
            {
                into.Add(error);
            }
        }
    }

    private static void ValidateDecimal(FieldModel field, DecimalFieldValue value, string path, List<FieldError> into)
    {
        var exact = value.ExactValue;
        if (field.Scale is { } scale && !FitsScale(exact, scale))
        {
            into.Add(new FieldError(path, "document.scale_exceeded", $"{field.Label} is limited to {scale} decimal place{(scale == 1 ? string.Empty : "s")}."));
        }
    }

    private static bool FitsScale(ExactDecimal exact, int scale)
    {
        // The parse path already refused more places; a value loaded from a document or
        // computed by the server can still carry them: the client reports, never rounds.
        var text = exact.ToCanonicalText();
        var dot = text.IndexOf('.');
        if (dot < 0)
        {
            return true;
        }

        var places = text[(dot + 1)..].TrimEnd('0').Length;
        return places <= scale;
    }

    /// <summary>The values a required field may not be left in: no value at all, an
    /// empty text (or select), or a link/attach reference with no id.</summary>
    internal static bool IsEmptyValue(FieldValue value) => value is NullFieldValue
        or TextFieldValue { Value.Length: 0 }
        or SelectFieldValue { Value.Length: 0 }
        or MultiSelectFieldValue { Values.Count: 0 }
        or LinkFieldValue { Id: null }
        or AttachFieldValue { FileId: null };

    private static bool IsEmailShape(string text)
    {
        var at = text.IndexOf('@');
        return at > 0
            && at < text.Length - 1
            && text.IndexOf('@', at + 1) < 0
            && !text.StartsWith('.')
            && !text.EndsWith('.');
    }

    private static bool IsUrlShape(string text) =>
        text.StartsWith("http://", StringComparison.Ordinal)
        || text.StartsWith("https://", StringComparison.Ordinal);
}

/// <summary>Path helpers shared with <see cref="FieldPath"/>: the table prefix form.</summary>
public static class TablePathExtensions
{
    /// <summary>The table-prefixed path (<c>items[3]</c>; rows counted from 1). A
    /// static class cannot carry an extension receiver, so this is a plain function
    /// next to <see cref="FieldPath.OfRow"/>.</summary>
    public static string OfTable(string tableName, int row) => $"{tableName}[{row}]";
}