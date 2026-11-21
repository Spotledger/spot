using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace Spotledger.Client.FlowDesigner;

/// One value of a node's config table, in the shapes the process text form allows: a string, a
/// whole number, a boolean, an array of strings, or — for a key this build cannot type — the
/// literal exactly as it was written, so an unknown kind still round-trips byte for byte
/// (ADR-080 extensibility).
public abstract record FlowValue
{

    public abstract string Render();

    public abstract string ToDisplayText();

    public static FlowValue Text(string value) => new FlowStringValue(value);

    public static FlowValue Number(long value) => new FlowIntegerValue(value);

    public static FlowValue Flag(bool value) => new FlowBooleanValue(value);

    public static FlowValue List(IReadOnlyList<string> items) =>
        new FlowStringListValue(items ?? throw new ArgumentNullException(nameof(items)));
}

/// A TOML basic string.
public sealed record FlowStringValue(string Value) : FlowValue
{

    public override string Render() => ProcessText.Quote(Value);

    public override string ToDisplayText() => Value;
}

/// A whole number.
public sealed record FlowIntegerValue(long Value) : FlowValue
{

    public override string Render() => Value.ToString(CultureInfo.InvariantCulture);

    public override string ToDisplayText() => Render();
}

/// A boolean.
public sealed record FlowBooleanValue(bool Value) : FlowValue
{

    public override string Render() => Value ? "true" : "false";

    public override string ToDisplayText() => Render();
}

/// An inline array of strings.
public sealed record FlowStringListValue(IReadOnlyList<string> Items) : FlowValue
{

    public bool Equals(FlowStringListValue? other)
    {
        if (other is null || other.Items.Count != Items.Count)
        {
            return false;
        }

        for (int i = 0; i < Items.Count; i++)
        {
            if (!string.Equals(other.Items[i], Items[i], StringComparison.Ordinal))
            {
                return false;
            }
        }

        return true;
    }

    public override int GetHashCode()
    {
        int hash = 17;
        foreach (string item in Items)
        {
            hash = (hash * 31) + StringComparer.Ordinal.GetHashCode(item);
        }

        return hash;
    }

    public override string Render()
    {
        StringBuilder builder = new("[");
        for (int i = 0; i < Items.Count; i++)
        {
            if (i > 0)
            {
                builder.Append(", ");
            }

            builder.Append(ProcessText.Quote(Items[i]));
        }

        return builder.Append(']').ToString();
    }

    public override string ToDisplayText()
    {
        // One item per line: the property panel edits a list in a plain multi-line box.
        StringBuilder builder = new();
        for (int i = 0; i < Items.Count; i++)
        {
            if (i > 0)
            {
                builder.Append('\n');
            }

            builder.Append(Items[i]);
        }

        return builder.ToString();
    }
}

/// A literal this build does not type, preserved as written.
public sealed record FlowRawValue(string Literal) : FlowValue
{

    public override string Render() => Literal;

    public override string ToDisplayText() => Literal;
}
