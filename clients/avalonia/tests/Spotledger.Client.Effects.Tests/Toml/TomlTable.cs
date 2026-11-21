using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Spotledger.Client.Effects.Tests.Toml;

/// <summary>A parsed TOML table, with the typed getters the corpus loader needs.</summary>
internal sealed class TomlTable
{
    private readonly Dictionary<string, object?> _values;

    internal TomlTable(Dictionary<string, object?> values) => _values = values;

    /// <summary>An empty table.</summary>
    public static TomlTable Empty { get; } = new(new Dictionary<string, object?>(StringComparer.Ordinal));

    /// <summary>The member names, in file order.</summary>
    public IReadOnlyCollection<string> Keys => _values.Keys;

    /// <summary>The member count.</summary>
    public int Count => _values.Count;

    /// <summary>Wraps a parsed node when it is a table.</summary>
    public static TomlTable? From(object? node) => node is Dictionary<string, object?> table ? new TomlTable(table) : null;

    /// <summary>Whether the member exists.</summary>
    public bool Has(string key) => _values.ContainsKey(key);

    /// <summary>The raw node of a member, when present.</summary>
    public object? Node(string key) => _values.TryGetValue(key, out object? value) ? value : null;

    /// <summary>A string member, or the fallback.</summary>
    public string? String(string key, string? fallback = null) =>
        _values.TryGetValue(key, out object? value) && value is string text ? text : fallback;

    /// <summary>A string member the corpus must carry.</summary>
    public string RequireString(string key) =>
        String(key) ?? throw new FormatException(string.Create(
            CultureInfo.InvariantCulture, $"the corpus needs a string `{key}`"));

    /// <summary>An integer member, or the fallback.</summary>
    public long Integer(string key, long fallback = 0) =>
        _values.TryGetValue(key, out object? value) && value is long number ? number : fallback;

    /// <summary>A boolean member, or the fallback.</summary>
    public bool Boolean(string key, bool fallback = false) =>
        _values.TryGetValue(key, out object? value) && value is bool flag ? flag : fallback;

    /// <summary>A table member, when present.</summary>
    public TomlTable? Table(string key) => From(Node(key));

    /// <summary>A table member the corpus must carry.</summary>
    public TomlTable RequireTable(string key) =>
        Table(key) ?? throw new FormatException(string.Create(
            CultureInfo.InvariantCulture, $"the corpus needs a table `{key}`"));

    /// <summary>An array member, when present and an array.</summary>
    public IReadOnlyList<object?>? ArrayOf(string key) =>
        _values.TryGetValue(key, out object? value) && value is List<object?> list ? list : null;

    /// <summary>The string elements of an array member (empty when absent).</summary>
    public IReadOnlyList<string> Strings(string key)
    {
        IReadOnlyList<object?>? list = ArrayOf(key);
        if (list is null)
        {
            return System.Array.Empty<string>();
        }

        var values = new List<string>(list.Count);
        foreach (object? item in list)
        {
            if (item is string text)
            {
                values.Add(text);
            }
        }

        return values;
    }

    /// <summary>The table elements of an array member (empty when absent).</summary>
    public IReadOnlyList<TomlTable> Tables(string key)
    {
        IReadOnlyList<object?>? list = ArrayOf(key);
        if (list is null)
        {
            return System.Array.Empty<TomlTable>();
        }

        var tables = new List<TomlTable>(list.Count);
        foreach (object? item in list)
        {
            if (From(item) is { } table)
            {
                tables.Add(table);
            }
        }

        return tables;
    }

    /// <summary>Every member as a plain node map (for JSON conversion).</summary>
    public IReadOnlyDictionary<string, object?> Members => _values;
}

/// <summary>Turns a parsed TOML node into JSON text: the protocol and the document are JSON,
/// the corpus is TOML, and this is the one conversion between them.</summary>
internal static class TomlJson
{
    /// <summary>The JSON text of one node.</summary>
    public static string Write(object? node)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            WriteNode(writer, node);
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>The JSON text of a whole table.</summary>
    public static string Write(TomlTable table)
    {
        ArgumentNullException.ThrowIfNull(table);
        return Write(table.Members);
    }

    private static void WriteNode(Utf8JsonWriter writer, object? node)
    {
        WriteNodeTo(writer, node);
    }

    /// <summary>Writes one node into an existing writer (the corpus loader injects row ids).</summary>
    public static void WriteNodeTo(Utf8JsonWriter writer, object? node)
    {
        switch (node)
        {
            case null:
                writer.WriteNullValue();
                break;
            case string text:
                writer.WriteStringValue(text);
                break;
            case long number:
                writer.WriteNumberValue(number);
                break;
            case bool flag:
                writer.WriteBooleanValue(flag);
                break;
            case Dictionary<string, object?> table:
                writer.WriteStartObject();
                foreach (KeyValuePair<string, object?> member in table)
                {
                    writer.WritePropertyName(member.Key);
                    WriteNode(writer, member.Value);
                }

                writer.WriteEndObject();
                break;
            case List<object?> list:
                writer.WriteStartArray();
                foreach (object? item in list)
                {
                    WriteNode(writer, item);
                }

                writer.WriteEndArray();
                break;
            default:
                throw new FormatException("a TOML node is a table, an array, a string, an integer or a boolean");
        }
    }
}
