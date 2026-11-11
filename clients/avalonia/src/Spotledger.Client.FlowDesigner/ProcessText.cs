using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace Spotledger.Client.FlowDesigner;

/// The scalar rendering and reading rules of the process text form, shared by the writer and the
/// reader so both agree with <c>spot-bpm-model</c>: a basic string escapes only
/// <c>" \ \n \t \r</c> and other control characters, and nothing else is rewritten.
public static class ProcessText
{

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
                case '\t':
                    builder.Append("\\t");
                    break;
                case '\r':
                    builder.Append("\\r");
                    break;
                default:
                    if (c < ' ')
                    {
                        builder.Append("\\u").Append(((int)c).ToString("X4", CultureInfo.InvariantCulture));
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

    public static string Unquote(string literal, int line = 0)
    {
        ArgumentNullException.ThrowIfNull(literal);
        if (literal.Length < 2 || literal[0] != '"' || literal[^1] != '"')
        {
            throw FlowTextException.At(line, "a string must be enclosed in double quotes");
        }

        StringBuilder builder = new(literal.Length);
        for (int i = 1; i < literal.Length - 1; i++)
        {
            char c = literal[i];
            if (c != '\\')
            {
                builder.Append(c);
                continue;
            }

            i++;
            if (i >= literal.Length - 1)
            {
                throw FlowTextException.At(line, "a string ends with a dangling escape");
            }

            char escape = literal[i];
            switch (escape)
            {
                case '"':
                    builder.Append('"');
                    break;
                case '\\':
                    builder.Append('\\');
                    break;
                case 'n':
                    builder.Append('\n');
                    break;
                case 't':
                    builder.Append('\t');
                    break;
                case 'r':
                    builder.Append('\r');
                    break;
                case 'u':
                    if (i + 4 >= literal.Length)
                    {
                        throw FlowTextException.At(line, "a \\u escape needs four hex digits");
                    }

                    string hex = literal.Substring(i + 1, 4);
                    if (!int.TryParse(hex, NumberStyles.HexNumber, CultureInfo.InvariantCulture, out int code))
                    {
                        throw FlowTextException.At(line, "a \\u escape needs four hex digits");
                    }

                    builder.Append((char)code);
                    i += 4;
                    break;
                default:
                    throw FlowTextException.At(line, "unknown escape sequence");
            }
        }

        return builder.ToString();
    }

    public static FlowValue ParseLiteral(string literal, int line = 0)
    {
        ArgumentNullException.ThrowIfNull(literal);
        string text = literal.Trim();
        if (text.Length == 0)
        {
            throw FlowTextException.At(line, "a value is missing");
        }

        if (text[0] == '"')
        {
            return FlowValue.Text(Unquote(text, line));
        }

        if (string.Equals(text, "true", StringComparison.Ordinal))
        {
            return FlowValue.Flag(true);
        }

        if (string.Equals(text, "false", StringComparison.Ordinal))
        {
            return FlowValue.Flag(false);
        }

        if (long.TryParse(text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long number))
        {
            return FlowValue.Number(number);
        }

        if (text[0] == '[')
        {
            return ParseStringArray(text, line);
        }

        return new FlowRawValue(text);
    }

    private static FlowValue ParseStringArray(string text, int line)
    {
        if (text[^1] != ']')
        {
            throw FlowTextException.At(line, "an array must close on the same line");
        }

        string inner = text[1..^1].Trim();
        if (inner.Length == 0)
        {
            return FlowValue.List([]);
        }

        List<string> items = [];
        foreach (string part in SplitItems(inner, line))
        {
            string item = part.Trim();
            if (item.Length == 0)
            {
                throw FlowTextException.At(line, "an array has an empty item");
            }

            items.Add(item[0] == '"' ? Unquote(item, line) : item);
        }

        return FlowValue.List(items);
    }

    private static IEnumerable<string> SplitItems(string inner, int line)
    {
        int start = 0;
        bool quoted = false;
        for (int i = 0; i < inner.Length; i++)
        {
            char c = inner[i];
            if (c == '\\' && quoted)
            {
                i++;
                continue;
            }

            if (c == '"')
            {
                quoted = !quoted;
                continue;
            }

            if (c == ',' && !quoted)
            {
                yield return inner[start..i];
                start = i + 1;
            }
        }

        if (quoted)
        {
            throw FlowTextException.At(line, "a string in an array is not closed");
        }

        yield return inner[start..];
    }
}
