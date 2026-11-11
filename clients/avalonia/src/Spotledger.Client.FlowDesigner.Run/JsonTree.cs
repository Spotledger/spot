using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Spotledger.Client.FlowDesigner.Run;

/// What a JSON value is, in the five shapes the panes draw.
public enum JsonKind
{
    Map,
    Sequence,
    Text,
    Number,
    Flag,
    Nothing,
}

/// One value of a payload: a scalar with its text, or a container with its children in the order
/// the server sent them. A number keeps the literal exactly as it arrived (never a floating-point
/// value: ADR-12), and a container remembers whether the pane has it open.
public sealed class JsonTreeNode
{

    private readonly List<JsonTreeNode> _children = [];

    internal JsonTreeNode(string name, JsonKind kind, string text)
    {
        Name = name;
        Kind = kind;
        Text = text;
    }

    public string Name { get; }

    public JsonKind Kind { get; }

    /// The scalar's text; empty for a container.
    public string Text { get; internal set; }

    public IReadOnlyList<JsonTreeNode> Children => _children;

    public bool Expanded { get; set; }

    /// True when this value was replaced by a redaction marker, so a pane can style it.
    public bool IsRedacted { get; internal set; }

    public bool IsContainer => Kind is JsonKind.Map or JsonKind.Sequence;

    public JsonTreeNode? Child(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        foreach (JsonTreeNode child in _children)
        {
            if (string.Equals(child.Name, name, StringComparison.Ordinal))
            {
                return child;
            }
        }

        return null;
    }

    internal void Add(JsonTreeNode child) => _children.Add(child);

    internal void Clear() => _children.Clear();
}

/// One line of a JSON tree pane: indentation, the path that identifies it (for expand/collapse),
/// the key, the kind and what to draw.
public sealed record JsonTreeRow(int Depth, string Path, string Name, JsonKind Kind, string Display, bool IsRedacted);

/// The result of reading a payload: either a tree, or the position of the offending byte. The pane
/// shows the message; the sample-data editor shows the position too.
public sealed record JsonParseResult(bool Ok, JsonTreeNode? Root, string? Error = null, long Line = 1, long Column = 1)
{

    public static JsonParseResult Failure(string error, long line, long column) => new(false, null, error, line, column);
}

/// Reading, walking and writing a JSON payload for the input and output panes. Values are never
/// converted to floating point and key order is the server's, so what a person compares is what
/// the server recorded.
public static class JsonTree
{

    public const string RootPath = "$";

    private static readonly JsonDocumentOptions Options = new()
    {
        AllowTrailingCommas = false,
        CommentHandling = JsonCommentHandling.Disallow,
        MaxDepth = 64,
    };

    /// Parses a payload. An empty document is an error, not an empty tree, so a blank pane is
    /// never confused with an empty object.
    public static JsonParseResult Parse(string? text)
    {
        if (string.IsNullOrWhiteSpace(text))
        {
            return JsonParseResult.Failure("The payload is empty.", 1, 1);
        }

        try
        {
            using JsonDocument document = JsonDocument.Parse(text, Options);
            JsonTreeNode root = Node(string.Empty, document.RootElement, 0);
            return new JsonParseResult(true, root);
        }
        catch (JsonException error)
        {
            long line = (error.LineNumber ?? 0) + 1;
            long column = (error.BytePositionInLine ?? 0) + 1;
            return JsonParseResult.Failure(error.Message, line, column);
        }
    }

    /// Whether a payload parses at all, without keeping the tree.
    public static bool IsValid(string? text) => Parse(text).Ok;

    /// A tree for a scalar value, used where the client builds one itself (a redacted value).
    public static JsonTreeNode Scalar(string name, JsonKind kind, string text) =>
        new(name, kind, text) { Expanded = false, IsRedacted = false };

    /// The rows of a pane: a container shows its children only while it is open.
    public static IReadOnlyList<JsonTreeRow> Rows(JsonTreeNode? root)
    {
        List<JsonTreeRow> rows = [];
        if (root is not null)
        {
            Walk(root, RootPath, 0, rows);
        }

        return rows;
    }

    /// Opens or closes one container by its path. Answers false when the path is not a container.
    public static bool Toggle(JsonTreeNode? root, string path)
    {
        JsonTreeNode? node = Find(root, path);
        if (node is null || !node.IsContainer)
        {
            return false;
        }

        node.Expanded = !node.Expanded;
        return true;
    }

    /// Finds a node by its pane path.
    public static JsonTreeNode? Find(JsonTreeNode? root, string? path)
    {
        if (root is null || string.IsNullOrEmpty(path))
        {
            return null;
        }

        if (string.Equals(path, RootPath, StringComparison.Ordinal))
        {
            return root;
        }

        if (!path.StartsWith(RootPath, StringComparison.Ordinal))
        {
            return null;
        }

        JsonTreeNode current = root;
        int index = RootPath.Length;
        while (index < path.Length && current.IsContainer)
        {
            if (path[index] == '.')
            {
                int end = path.IndexOfAny(['.', '['], index + 1);
                string name = end < 0 ? path[(index + 1)..] : path[(index + 1)..end];
                JsonTreeNode? next = current.Child(name);
                if (next is null)
                {
                    return null;
                }

                current = next;
                index = end < 0 ? path.Length : end;
            }
            else if (path[index] == '[')
            {
                int end = path.IndexOf(']', index + 1);
                if (end < 0 || !int.TryParse(path[(index + 1)..end], NumberStyles.Integer, CultureInfo.InvariantCulture, out int slot))
                {
                    return null;
                }

                if (slot < 0 || slot >= current.Children.Count)
                {
                    return null;
                }

                current = current.Children[slot];
                index = end + 1;
            }
            else
            {
                return null;
            }
        }

        return current;
    }

    /// The payload as indented text, every container open: what a pane can copy out, and what the
    /// tests compare. Markers stay exactly as they were written.
    public static string ToText(JsonTreeNode? root)
    {
        if (root is null)
        {
            return string.Empty;
        }

        StringBuilder builder = new();
        Write(builder, root, 0, withName: false);
        return builder.ToString();
    }

    /// A JSON string literal for display: quotes and the four escapes a payload can carry.
    public static string Quote(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        StringBuilder builder = new(text.Length + 2);
        builder.Append('"');
        foreach (char c in text)
        {
            switch (c)
            {
                case '"':
                    builder.Append("\\\"");
                    break;
                case '\\':
                    builder.Append("\\\\");
                    break;
                case '\n':
                    builder.Append("\\n");
                    break;
                case '\r':
                    builder.Append("\\r");
                    break;
                case '\t':
                    builder.Append("\\t");
                    break;
                default:
                    if (c < ' ')
                    {
                        builder.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                    }
                    else
                    {
                        builder.Append(c);
                    }

                    break;
            }
        }

        return builder.Append('"').ToString();
    }

    private static JsonTreeNode Node(string name, JsonElement element, int depth)
    {
        switch (element.ValueKind)
        {
            case JsonValueKind.Object:
                JsonTreeNode map = new(name, JsonKind.Map, string.Empty) { Expanded = depth < 2 };
                foreach (JsonProperty property in element.EnumerateObject())
                {
                    map.Add(Node(property.Name, property.Value, depth + 1));
                }

                return map;
            case JsonValueKind.Array:
                JsonTreeNode list = new(name, JsonKind.Sequence, string.Empty) { Expanded = depth < 2 };
                int slot = 0;
                foreach (JsonElement item in element.EnumerateArray())
                {
                    list.Add(Node("[" + slot.ToString(CultureInfo.InvariantCulture) + "]", item, depth + 1));
                    slot++;
                }

                return list;
            case JsonValueKind.String:
                return new JsonTreeNode(name, JsonKind.Text, element.GetString() ?? string.Empty);
            case JsonValueKind.Number:
                return new JsonTreeNode(name, JsonKind.Number, element.GetRawText());
            case JsonValueKind.True:
                return new JsonTreeNode(name, JsonKind.Flag, "true");
            case JsonValueKind.False:
                return new JsonTreeNode(name, JsonKind.Flag, "false");
            default:
                return new JsonTreeNode(name, JsonKind.Nothing, "null");
        }
    }

    private static void Walk(JsonTreeNode node, string path, int depth, List<JsonTreeRow> rows)
    {
        rows.Add(new JsonTreeRow(depth, path, node.Name, node.Kind, Display(node), node.IsRedacted));
        if (!node.IsContainer || !node.Expanded)
        {
            return;
        }

        for (int i = 0; i < node.Children.Count; i++)
        {
            JsonTreeNode child = node.Children[i];
            string childPath = child.Name.StartsWith('[') && node.Kind == JsonKind.Sequence
                ? path + child.Name
                : path + "." + child.Name;
            Walk(child, childPath, depth + 1, rows);
        }
    }

    private static string Display(JsonTreeNode node) => node.Kind switch
    {
        JsonKind.Map => node.Expanded
            ? "{ " + node.Children.Count.ToString(CultureInfo.InvariantCulture) + " keys }"
            : "{ … }",
        JsonKind.Sequence => node.Expanded
            ? "[ " + node.Children.Count.ToString(CultureInfo.InvariantCulture) + " items ]"
            : "[ … ]",
        JsonKind.Text => node.Text,
        _ => node.Text,
    };

    private static void Write(StringBuilder builder, JsonTreeNode node, int depth, bool withName)
    {
        string indent = new(' ', depth * 2);
        if (withName)
        {
            builder.Append(indent).Append(Quote(node.Name)).Append(": ");
        }

        switch (node.Kind)
        {
            case JsonKind.Map:
            case JsonKind.Sequence:
                bool isObject = node.Kind == JsonKind.Map;
                builder.Append(isObject ? '{' : '[').Append('\n');
                for (int i = 0; i < node.Children.Count; i++)
                {
                    JsonTreeNode child = node.Children[i];
                    if (isObject)
                    {
                        Write(builder, child, depth + 1, withName: true);
                    }
                    else
                    {
                        builder.Append(new string(' ', (depth + 1) * 2));
                        Write(builder, child, depth + 1, withName: false);
                    }

                    if (i < node.Children.Count - 1)
                    {
                        builder.Append(',');
                    }

                    builder.Append('\n');
                }

                builder.Append(indent).Append(isObject ? '}' : ']');
                break;
            case JsonKind.Text:
                builder.Append(Quote(node.Text));
                break;
            default:
                builder.Append(node.Text);
                break;
        }
    }
}
