using System.Text.Json;
using Microsoft.Kiota.Abstractions.Serialization;

namespace Spotledger.Client.Metadata;

/// <summary>A closed, optional presentation preference; Unknown means use the default control.</summary>
public enum ControlHint
{
    Unknown, Switch, Checkbox, Dropdown, Radio, Segmented, Autocomplete, Chips, Textarea,
}

/// <summary>Typed ADR-068 format parameters, independent of any renderer or document value.</summary>
public sealed record FieldFormatParameters(
    string? CurrencyField = null, string? Symbology = null, string? Syntax = null, long? Max = null)
{
    internal static FieldFormatParameters Decode(IDictionary<string, object>? values)
    {
        object? Get(string key) => values is not null && values.TryGetValue(key, out var value) ? value : null;
        static string? Text(object? value) => value switch
        {
            string text => text,
            UntypedString text => text.GetValue(),
            JsonElement { ValueKind: JsonValueKind.String } json => json.GetString(),
            _ => null,
        };
        static long? Integer(object? value) => value switch
        {
            int number => number,
            long number => number,
            UntypedInteger number => number.GetValue(),
            UntypedLong number => number.GetValue(),
            JsonElement { ValueKind: JsonValueKind.Number } json when json.TryGetInt64(out var number) => number,
            UntypedDecimal number when decimal.Truncate(number.GetValue()) == number.GetValue()
                && number.GetValue() >= long.MinValue && number.GetValue() <= long.MaxValue => (long)number.GetValue(),
            decimal number when decimal.Truncate(number) == number && number >= long.MinValue && number <= long.MaxValue => (long)number,
            _ => null,
        };
        return new(Text(Get("currency_field")), Text(Get("symbology")), Text(Get("syntax")), Integer(Get("max")));
    }
}

/// <summary>Server-resolved currency presentation for one field path; never part of a value or write.</summary>
public sealed record CurrencyInfo(string Code, string Symbol);

/// <summary>The server-resolved upload limit for this caller; an empty Accept permits any media type.</summary>
public sealed record AttachLimit(long MaxBytes, IReadOnlyList<string> Accept);

/// <summary>Decoded permissions for one DocType. Lists are permission hints, not authorization.</summary>
public sealed record DocTypePermissionState(
    bool CanCreate,
    IReadOnlyList<string> Hidden,
    IReadOnlyList<string> ReadOnly,
    IReadOnlyList<string> Masked,
    IReadOnlyList<string> Revealable,
    IReadOnlyDictionary<string, AttachLimit> AttachLimits);

/// <summary>A renderer-neutral permission snapshot from auth_permissions.</summary>
public sealed class PermissionSnapshot
{
    private PermissionSnapshot(string? etag, IReadOnlyDictionary<string, DocTypePermissionState> doctypes)
    {
        ETag = etag;
        DocTypes = doctypes;
    }

    /// <summary>The opaque permission snapshot version, not a document or metadata version.</summary>
    public string? ETag { get; }
    /// <summary>Permissions keyed by target DocType, including link targets.</summary>
    public IReadOnlyDictionary<string, DocTypePermissionState> DocTypes { get; }

    /// <summary>Decodes the complete auth_permissions response. Absent additive properties use plain defaults.</summary>
    public static PermissionSnapshot Decode(string json)
    {
        using var document = JsonDocument.Parse(json);
        var root = document.RootElement;
        var doctypes = new Dictionary<string, DocTypePermissionState>(StringComparer.Ordinal);
        if (root.TryGetProperty("data", out var data) && data.TryGetProperty("doctypes", out var list))
        {
            foreach (var item in list.EnumerateArray())
            {
                var limits = new Dictionary<string, AttachLimit>(StringComparer.Ordinal);
                if (item.TryGetProperty("attach_limits", out var attachments))
                {
                    foreach (var limit in attachments.EnumerateObject())
                    {
                        limits[limit.Name] = new(limit.Value.GetProperty("max_bytes").GetInt64(), Strings(limit.Value, "accept"));
                    }
                }
                item.TryGetProperty("fields", out var fields);
                var create = item.TryGetProperty("rights", out var rights)
                    && rights.TryGetProperty("create", out var allowed) && allowed.ValueKind == JsonValueKind.True;
                doctypes[item.GetProperty("doctype").GetString()!] = new(create,
                    Strings(fields, "hidden"), Strings(fields, "read_only"), Strings(fields, "masked"),
                    Strings(fields, "revealable"), new System.Collections.ObjectModel.ReadOnlyDictionary<string, AttachLimit>(limits));
            }
        }
        return new(root.TryGetProperty("etag", out var etag) ? etag.GetString() : null,
            new System.Collections.ObjectModel.ReadOnlyDictionary<string, DocTypePermissionState>(doctypes));
    }

    private static IReadOnlyList<string> Strings(JsonElement element, string name) =>
        element.ValueKind == JsonValueKind.Object && element.TryGetProperty(name, out var list)
            ? Array.AsReadOnly(list.EnumerateArray().Select(value => value.GetString()!).ToArray())
            : Array.Empty<string>();
}
