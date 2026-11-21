using System.Text.Json;

namespace Spotledger.AdminCenter;

public enum SettingRisk
{
    Low,
    Medium,
    High,
}

/// <summary>One setting as the schema declares it (the <c>spot-settings</c> screen model).</summary>
public sealed record SettingDescriptor(
    string Key,
    string Type,
    string Default,
    string Description,
    bool RestartNeeded,
    SettingRisk Risk);

/// <summary>The current value of a setting and the scope it is inherited from (null when set here).</summary>
public sealed record SettingValue(string Value, string? InheritedFrom = null);

/// <summary>One rendered row of the settings screen.</summary>
public sealed record SettingRow(
    SettingDescriptor Descriptor,
    string Value,
    string? InheritedFrom,
    bool DiffersFromDefault,
    string? Error);

public static class SettingsSchema
{
    /// <summary>Parses the schema JSON served by the management API. Unknown fields are ignored.</summary>
    public static IReadOnlyList<SettingDescriptor> Parse(string json)
    {
        using var document = JsonDocument.Parse(json);
        var result = new List<SettingDescriptor>();
        foreach (var item in document.RootElement.GetProperty("settings").EnumerateArray())
        {
            result.Add(new SettingDescriptor(
                item.GetProperty("key").GetString() ?? throw new JsonException("A setting has no key."),
                Text(item, "type") ?? "string",
                Text(item, "default") ?? string.Empty,
                Text(item, "description") ?? string.Empty,
                item.TryGetProperty("restart_needed", out var restart) && restart.ValueKind == JsonValueKind.True,
                ParseRisk(Text(item, "risk"))));
        }

        return result;
    }

    private static string? Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind != JsonValueKind.Null ? value.ToString() : null;

    private static SettingRisk ParseRisk(string? text) =>
        Enum.TryParse<SettingRisk>(text, ignoreCase: true, out var risk) ? risk : SettingRisk.Low;
}

/// <summary>
/// The settings screen generated from the schema: every descriptor becomes a row with no
/// per-setting UI code. Supports search, a differs-from-default filter, inheritance and
/// restart/risk badges, and inline validation errors returned by the server.
/// </summary>
public sealed class SettingsScreenModel
{
    private readonly IReadOnlyList<SettingDescriptor> _schema;
    private readonly IReadOnlyDictionary<string, SettingValue> _values;
    private readonly Dictionary<string, string> _errors = new(StringComparer.Ordinal);

    public SettingsScreenModel(IReadOnlyList<SettingDescriptor> schema, IReadOnlyDictionary<string, SettingValue> values)
    {
        ArgumentNullException.ThrowIfNull(schema);
        ArgumentNullException.ThrowIfNull(values);
        _schema = schema;
        _values = values;
    }

    public string Search { get; set; } = string.Empty;

    public bool OnlyDifferingFromDefault { get; set; }

    public void SetServerErrors(IReadOnlyDictionary<string, string> errors)
    {
        _errors.Clear();
        foreach (var (key, message) in errors)
        {
            _errors[key] = message;
        }
    }

    public IReadOnlyList<SettingRow> Rows()
    {
        var rows = new List<SettingRow>();
        foreach (var descriptor in _schema)
        {
            var current = _values.TryGetValue(descriptor.Key, out var value) ? value : new SettingValue(descriptor.Default);
            var differs = !string.Equals(current.Value, descriptor.Default, StringComparison.Ordinal);
            if (OnlyDifferingFromDefault && !differs)
            {
                continue;
            }

            if (Search.Length > 0
                && !descriptor.Key.Contains(Search, StringComparison.OrdinalIgnoreCase)
                && !descriptor.Description.Contains(Search, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            rows.Add(new SettingRow(descriptor, current.Value, current.InheritedFrom, differs, _errors.GetValueOrDefault(descriptor.Key)));
        }

        return rows;
    }
}
