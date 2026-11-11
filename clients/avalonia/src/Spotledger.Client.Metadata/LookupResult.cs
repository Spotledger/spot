using System.Collections.ObjectModel;
using System.Text.Json;

namespace Spotledger.Client.Metadata;

/// <summary>A lookup display column; neither its cells nor its label are write values.</summary>
public sealed record LookupColumn(string Name, string Label);

/// <summary>A candidate with display-only cells and separately named copyable extras.</summary>
public sealed record LookupResultRow(string Id, string Label, string? Description,
    IReadOnlyDictionary<string, string> Cells, IReadOnlyDictionary<string, string> ExtrasJson);

/// <summary>Renderer-neutral lookup_search result. Empty Columns retains the single-column presentation.</summary>
public sealed record LookupResult(IReadOnlyList<LookupResultRow> Rows, IReadOnlyList<LookupColumn> Columns,
    bool HasMore, string? Cursor)
{
    /// <summary>Decodes a complete lookup_search response, retaining extras as exact JSON, not display cells.</summary>
    public static LookupResult Decode(string json)
    {
        using var document = JsonDocument.Parse(json);
        var data = document.RootElement.GetProperty("data");
        var columns = data.TryGetProperty("columns", out var columnArray)
            ? columnArray.EnumerateArray().Select(column => new LookupColumn(column.GetProperty("name").GetString()!, column.GetProperty("label").GetString()!)).ToArray()
            : Array.Empty<LookupColumn>();
        var rows = data.GetProperty("rows").EnumerateArray().Select(row => new LookupResultRow(
            row.GetProperty("id").GetString()!, row.GetProperty("label").GetString()!,
            row.TryGetProperty("description", out var description) ? description.GetString() : null,
            Map(row, "cells", value => value.GetString()!), Map(row, "extras", value => value.GetRawText()))).ToArray();
        return new(Array.AsReadOnly(rows), Array.AsReadOnly(columns), data.GetProperty("has_more").GetBoolean(),
            data.TryGetProperty("cursor", out var cursor) ? cursor.GetString() : null);
    }

    private static ReadOnlyDictionary<string, string> Map(JsonElement element, string name, Func<JsonElement, string> decode) =>
        new ReadOnlyDictionary<string, string>(element.TryGetProperty(name, out var map)
            ? map.EnumerateObject().ToDictionary(pair => pair.Name, pair => decode(pair.Value), StringComparer.Ordinal)
            : new Dictionary<string, string>(StringComparer.Ordinal));
}
