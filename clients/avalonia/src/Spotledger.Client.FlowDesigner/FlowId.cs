using System;
using System.Text;

namespace Spotledger.Client.FlowDesigner;

/// The one identifier grammar of the process model: a lower-case letter first, then lower-case
/// letters, digits and underscores, at most 64 characters — the same shape the server enforces
/// for a node, a rule and a process name.
public static class FlowId
{

    public const int MaxLength = 64;

    public static bool IsName(string? value)
    {
        if (string.IsNullOrEmpty(value) || value.Length > MaxLength)
        {
            return false;
        }

        if (value[0] < 'a' || value[0] > 'z')
        {
            return false;
        }

        foreach (char c in value)
        {
            bool ok = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_';
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }

    public static string Require(string? value, string parameter)
    {
        if (value is not { } name || !IsName(name))
        {
            throw new ArgumentException(
                "An id must match [a-z][a-z0-9_]{0,63}: " + (value ?? "(null)"),
                parameter);
        }

        return name;
    }

    public static string Sanitize(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        StringBuilder builder = new(text.Length);
        foreach (char raw in text.Trim())
        {
            char c = char.ToLowerInvariant(raw);
            bool ok = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_';
            if (!ok)
            {
                builder.Append('_');
                continue;
            }

            builder.Append(c);
        }

        while (builder.Length > 0 && (builder[0] < 'a' || builder[0] > 'z'))
        {
            builder.Remove(0, 1);
        }

        while (builder.Length > 0 && builder[^1] == '_')
        {
            builder.Length--;
        }

        if (builder.Length == 0)
        {
            return "node";
        }

        if (builder.Length > MaxLength)
        {
            builder.Length = MaxLength;
        }

        return builder.ToString();
    }
}
