using System.Text.Json;

namespace Spotledger.Client.Lists;

/// <summary>A filter scalar: a string, a 64-bit integer or a boolean (the contract's
/// <c>[string, integer, boolean]</c>). Never null: emptiness is <see cref="NullFilter"/>.</summary>
public readonly record struct ListScalar
{
    private ListScalar(char kind, string? text, long number, bool flag)
    {
        Kind = kind;
        Text = text;
        Number = number;
        Flag = flag;
    }

    /// <summary>'s' string, 'i' integer, 'b' boolean.</summary>
    public char Kind { get; }

    /// <summary>The string value ('s').</summary>
    public string? Text { get; }

    /// <summary>The integer value ('i').</summary>
    public long Number { get; }

    /// <summary>The boolean value ('b').</summary>
    public bool Flag { get; }

    /// <summary>A string scalar.</summary>
    public static ListScalar Of(string text) => new('s', text ?? throw new ArgumentNullException(nameof(text)), 0, false);

    /// <summary>An integer scalar.</summary>
    public static ListScalar Of(long number) => new('i', null, number, false);

    /// <summary>A boolean scalar.</summary>
    public static ListScalar Of(bool flag) => new('b', null, 0, flag);

    internal void Write(Utf8JsonWriter writer)
    {
        switch (Kind)
        {
            case 'i': writer.WriteNumberValue(Number); break;
            case 'b': writer.WriteBooleanValue(Flag); break;
            default: writer.WriteStringValue(Text); break;
        }
    }
}

/// <summary>Comparison operators of the contract's filter.</summary>
public enum CompareOp
{
    /// <summary>Equal.</summary>
    Eq,
    /// <summary>Not equal.</summary>
    Ne,
    /// <summary>Less than.</summary>
    Lt,
    /// <summary>Less or equal.</summary>
    Le,
    /// <summary>Greater than.</summary>
    Gt,
    /// <summary>Greater or equal.</summary>
    Ge,
}

/// <summary>Literal text operators of the contract's filter.</summary>
public enum TextOp
{
    /// <summary>Starts with.</summary>
    Prefix,
    /// <summary>Contains.</summary>
    Contains,
    /// <summary>Ends with.</summary>
    Suffix,
}

/// <summary>The structured filter the list sends. It mirrors the contract's <c>Filter</c> schema
/// one to one; a filter is data, never a string that is parsed or concatenated.</summary>
public abstract record ListFilter;

/// <summary>All of the items.</summary>
public sealed record AndFilter(IReadOnlyList<ListFilter> Items) : ListFilter;

/// <summary>Any of the items.</summary>
public sealed record OrFilter(IReadOnlyList<ListFilter> Items) : ListFilter;

/// <summary>Negation.</summary>
public sealed record NotFilter(ListFilter Item) : ListFilter;

/// <summary>A comparison of a field with a scalar.</summary>
public sealed record CompareFilter(string Field, CompareOp Op, ListScalar Value) : ListFilter;

/// <summary>Membership in a list of scalars.</summary>
public sealed record InFilter(string Field, IReadOnlyList<ListScalar> Values) : ListFilter;

/// <summary>A literal text match.</summary>
public sealed record TextFilter(string Field, TextOp Op, string Value, bool CaseInsensitive) : ListFilter;

/// <summary>Null test.</summary>
public sealed record NullFilter(string Field, bool IsNull) : ListFilter;

/// <summary>One order key.</summary>
public sealed record ListOrder(string Field, bool Descending);

/// <summary>The structural limits the list keeps so it never builds a filter the server refuses
/// (ADR-069: the HTTP filter is capped at 16 levels and 20 nodes; an <c>in</c> list at 1,000).</summary>
public static class ListLimits
{
    /// <summary>Deepest filter tree.</summary>
    public const int MaxDepth = 16;

    /// <summary>Most nodes in one filter.</summary>
    public const int MaxNodes = 20;

    /// <summary>Most entries in one <c>in</c> list.</summary>
    public const int MaxInValues = 1000;

    /// <summary>Most order keys.</summary>
    public const int MaxOrderKeys = 8;

    /// <summary>The most rows a page may ask for.</summary>
    public const int MaxPageSize = 500;

    /// <summary>The contract's identifier pattern.</summary>
    public static bool IsIdentifier(string name)
    {
        if (string.IsNullOrEmpty(name) || name.Length > 63 || name[0] is < 'a' or > 'z')
        {
            return false;
        }

        foreach (var c in name)
        {
            if (!(c is >= 'a' and <= 'z' or >= '0' and <= '9' or '_'))
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>Counts nodes and depth with an explicit stack; true when within the limits.</summary>
    public static bool Fits(ListFilter? filter)
    {
        if (filter is null)
        {
            return true;
        }

        var nodes = 0;
        var stack = new Stack<(ListFilter Node, int Depth)>();
        stack.Push((filter, 1));
        while (stack.Count > 0)
        {
            var (node, depth) = stack.Pop();
            if (++nodes > MaxNodes || depth > MaxDepth)
            {
                return false;
            }

            switch (node)
            {
                case AndFilter and:
                    if (and.Items.Count == 0) return false;
                    foreach (var item in and.Items) stack.Push((item, depth + 1));
                    break;
                case OrFilter or:
                    if (or.Items.Count == 0) return false;
                    foreach (var item in or.Items) stack.Push((item, depth + 1));
                    break;
                case NotFilter not:
                    stack.Push((not.Item, depth + 1));
                    break;
                case InFilter list when list.Values.Count is 0 or > MaxInValues:
                    return false;
            }
        }

        return true;
    }
}

/// <summary>One list query: the columns, the filter and the order. Immutable; a change makes a
/// new value, which is what lets a response for an older query be recognised and dropped.</summary>
public sealed record ListQuery(
    IReadOnlyList<string> Fields,
    ListFilter? Filter = null,
    IReadOnlyList<ListOrder>? Order = null)
{
    /// <summary>The order keys (never null).</summary>
    public IReadOnlyList<ListOrder> OrderKeys => Order ?? [];

    /// <summary>Writes the contract's <c>QueryRequest</c> body for one page.</summary>
    public string ToRequestJson(int limit, string? cursor, bool withTotal)
    {
        if (limit is < 1 or > ListLimits.MaxPageSize)
        {
            throw new ArgumentOutOfRangeException(nameof(limit));
        }

        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            writer.WriteStartArray("fields");
            foreach (var field in Fields)
            {
                Require(field);
                writer.WriteStringValue(field);
            }

            writer.WriteEndArray();
            if (Filter is not null)
            {
                if (!ListLimits.Fits(Filter))
                {
                    throw new InvalidOperationException("The filter is larger than the server accepts.");
                }

                writer.WritePropertyName("filter");
                WriteFilter(writer, Filter);
            }

            if (OrderKeys.Count > 0)
            {
                writer.WriteStartArray("order");
                foreach (var key in OrderKeys.Take(ListLimits.MaxOrderKeys))
                {
                    Require(key.Field);
                    writer.WriteStartObject();
                    writer.WriteString("field", key.Field);
                    writer.WriteString("dir", key.Descending ? "desc" : "asc");
                    writer.WriteEndObject();
                }

                writer.WriteEndArray();
            }

            writer.WriteNumber("limit", limit);
            if (cursor is not null)
            {
                writer.WriteString("cursor", cursor);
            }

            if (withTotal)
            {
                writer.WriteBoolean("with_total", true);
            }

            writer.WriteEndObject();
        }

        return System.Text.Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>Writes just the filter (golden tests, export).</summary>
    public static string FilterJson(ListFilter filter)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            WriteFilter(writer, filter);
        }

        return System.Text.Encoding.UTF8.GetString(stream.ToArray());
    }

    private static void Require(string field)
    {
        if (!ListLimits.IsIdentifier(field))
        {
            throw new ArgumentException("Not a field identifier.", nameof(field));
        }
    }

    private static void WriteFilter(Utf8JsonWriter writer, ListFilter filter)
    {
        // Depth is bounded by ListLimits.Fits (16) before any write.
        switch (filter)
        {
            case AndFilter and:
                writer.WriteStartObject();
                writer.WriteStartArray("and");
                foreach (var item in and.Items) WriteFilter(writer, item);
                writer.WriteEndArray();
                writer.WriteEndObject();
                break;
            case OrFilter or:
                writer.WriteStartObject();
                writer.WriteStartArray("or");
                foreach (var item in or.Items) WriteFilter(writer, item);
                writer.WriteEndArray();
                writer.WriteEndObject();
                break;
            case NotFilter not:
                writer.WriteStartObject();
                writer.WritePropertyName("not");
                WriteFilter(writer, not.Item);
                writer.WriteEndObject();
                break;
            case CompareFilter compare:
                Require(compare.Field);
                writer.WriteStartObject();
                writer.WriteString("field", compare.Field);
                writer.WriteString("op", compare.Op.ToString().ToLowerInvariant());
                writer.WritePropertyName("value");
                compare.Value.Write(writer);
                writer.WriteEndObject();
                break;
            case InFilter list:
                Require(list.Field);
                writer.WriteStartObject();
                writer.WriteString("field", list.Field);
                writer.WriteString("op", "in");
                writer.WriteStartArray("value");
                foreach (var value in list.Values) value.Write(writer);
                writer.WriteEndArray();
                writer.WriteEndObject();
                break;
            case TextFilter text:
                Require(text.Field);
                writer.WriteStartObject();
                writer.WriteString("field", text.Field);
                writer.WriteString("op", text.Op.ToString().ToLowerInvariant());
                writer.WriteString("value", text.Value);
                if (text.CaseInsensitive)
                {
                    writer.WriteBoolean("case_insensitive", true);
                }

                writer.WriteEndObject();
                break;
            case NullFilter empty:
                Require(empty.Field);
                writer.WriteStartObject();
                writer.WriteString("field", empty.Field);
                writer.WriteString("op", empty.IsNull ? "is_null" : "is_not_null");
                writer.WriteEndObject();
                break;
            default:
                throw new ArgumentException("Unknown filter node.", nameof(filter));
        }
    }
}
