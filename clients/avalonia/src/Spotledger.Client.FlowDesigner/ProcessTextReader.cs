using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace Spotledger.Client.FlowDesigner;

/// Reads the process text form into a <see cref="FlowDocument"/>.
///
/// The reader accepts exactly the shape the server writes: the header keys, <c>[[nodes]]</c>,
/// <c>[[edges]]</c> and <c>[[rules]]</c>. A top-level section this build does not know (for
/// example the participants section a later work package adds) is skipped, and a config key this
/// build cannot type is kept as written, so an older client never loses a newer server's data
/// (ADR-080). A malformed document — a missing required key, a value of the wrong shape, an
/// unknown key on an edge or a rule — is refused with its line number rather than guessed at.
public static class ProcessTextReader
{

    private static readonly string[] NodeCommonKeys =
    [
        "id",
        "kind",
        "join",
        "join_count",
        "terminal",
        "error",
        "timeout_secs",
        "retry_attempts",
        "retry_delay_secs",
    ];

    public static FlowDocument Read(string text) => Read(text, ProcessTextLimits.Default);

    public static FlowDocument Read(string text, ProcessTextLimits limits)
    {
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(limits);
        if (text.Length > limits.MaxTextLength)
        {
            throw FlowTextException.AtLimit(1, "the document is longer", ProcessTextLimits.Describe("text size", limits.MaxTextLength));
        }

        List<Section> sections = Split(text, limits);
        Section? root = null;
        foreach (Section section in sections)
        {
            if (section.Kind == SectionKind.Root)
            {
                root = section;
                break;
            }
        }

        if (root is null)
        {
            throw FlowTextException.At(0, "the document is empty");
        }

        string name = RequiredText(root, "name");
        string app = RequiredText(root, "app");
        FlowDocument document = new(name, app)
        {
            Title = OptionalText(root, "title"),
            Version = (int)RequiredInteger(root, "version", 1, int.MaxValue),
            Status = ParseStatus(RequiredText(root, "status"), root.LineOf("status")),
        };

        foreach (Section section in sections)
        {
            switch (section.Kind)
            {
                case SectionKind.Node:
                    document.AddNode(BuildNode(section));
                    break;
                case SectionKind.Edge:
                    document.AddEdge(BuildEdge(section));
                    break;
                case SectionKind.Rule:
                    document.AddRule(BuildRule(section));
                    break;
                case SectionKind.Root:
                case SectionKind.Unknown:
                default:
                    break;
            }
        }

        return document;
    }

    private static FlowNode BuildNode(Section section)
    {
        string rawId = RequiredText(section, "id");
        if (!FlowId.IsName(rawId))
        {
            throw FlowTextException.AtKey(section.LineOf("id"), "id", "is not a valid id");
        }

        string kind = RequiredText(section, "kind");
        if (kind.Length == 0)
        {
            throw FlowTextException.At(section.LineOf("kind"), "`kind` must not be empty");
        }

        FlowNode node = new(rawId, kind)
        {
            Join = ParseJoin(section),
            JoinCount = section.JoinCount,
            Terminal = OptionalFlag(section, "terminal"),
            Error = OptionalText(section, "error"),
            TimeoutSecs = section.Has("timeout_secs") ? (ulong)RequiredInteger(section, "timeout_secs", 0, long.MaxValue) : null,
            RetryAttempts = (int)(section.Has("retry_attempts") ? RequiredInteger(section, "retry_attempts", 1, int.MaxValue) : 1),
            RetryDelaySecs = section.Has("retry_delay_secs") ? (ulong)RequiredInteger(section, "retry_delay_secs", 0, long.MaxValue) : 0,
        };

        if (node.Error is { } error && !FlowId.IsName(error))
        {
            throw FlowTextException.AtKey(section.LineOf("error"), "error", "is not a valid id");
        }

        foreach (KeyValuePair<string, (string Literal, int Line)> pair in section.Values)
        {
            if (Array.IndexOf(NodeCommonKeys, pair.Key) >= 0)
            {
                continue;
            }

            node.Map[pair.Key] = ProcessText.ParseLiteral(pair.Value.Literal, pair.Value.Line);
        }

        return node;
    }

    private static FlowEdge BuildEdge(Section section)
    {
        RejectExtra(section, ["from", "to", "condition", "rule", "label", "default"]);
        string from = RequireId(section, "from");
        string to = RequireId(section, "to");
        return new FlowEdge(from, to)
        {
            Condition = OptionalText(section, "condition"),
            Rule = OptionalText(section, "rule"),
            Label = OptionalText(section, "label"),
            Default = OptionalFlag(section, "default"),
        };
    }

    private static FlowRule BuildRule(Section section)
    {
        RejectExtra(section, ["id", "expression"]);
        string id = RequireId(section, "id");
        return new FlowRule(id, RequiredText(section, "expression"));
    }

    private static string RequireId(Section section, string key)
    {
        string value = RequiredText(section, key);
        if (!FlowId.IsName(value))
        {
            throw FlowTextException.AtKey(section.LineOf(key), key, "is not a valid id");
        }

        return value;
    }

    private static FlowJoinKind ParseJoin(Section section)
    {
        string? raw = OptionalText(section, "join");
        switch (raw)
        {
            case null:
            case "all":
                return FlowJoinKind.All;
            case "any":
                return FlowJoinKind.Any;
            case "quorum":
                section.JoinCount = (int)RequiredInteger(section, "join_count", 1, int.MaxValue);
                return FlowJoinKind.Quorum;
            default:
                throw FlowTextException.At(section.LineOf("join"), "unknown join rule");
        }
    }

    private static string RequiredText(Section section, string key)
    {
        string? value = OptionalText(section, key);
        if (value is null)
        {
            throw FlowTextException.AtKey(section.Line, key, "must be a string");
        }

        return value;
    }

    private static string? OptionalText(Section section, string key)
    {
        if (!section.Values.TryGetValue(key, out (string Literal, int Line) entry))
        {
            return null;
        }

        string literal = entry.Literal.Trim();
        if (literal.Length == 0 || literal[0] != '"')
        {
            throw FlowTextException.AtKey(entry.Line, key, "must be a string");
        }

        return ProcessText.Unquote(literal, entry.Line);
    }

    private static bool OptionalFlag(Section section, string key)
    {
        if (!section.Values.TryGetValue(key, out (string Literal, int Line) entry))
        {
            return false;
        }

        string literal = entry.Literal.Trim();
        if (string.Equals(literal, "true", StringComparison.Ordinal))
        {
            return true;
        }

        if (string.Equals(literal, "false", StringComparison.Ordinal))
        {
            return false;
        }

        throw FlowTextException.AtKey(entry.Line, key, "must be a boolean");
    }

    private static long RequiredInteger(Section section, string key, long min, long max)
    {
        string? literal = section.Has(key) ? section.Values[key].Literal.Trim() : null;
        if (literal is null
            || !long.TryParse(literal, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long value))
        {
            throw FlowTextException.AtKey(section.LineOf(key), key, "must be an integer");
        }

        if (value < min || value > max)
        {
            throw FlowTextException.AtKey(section.LineOf(key), key, "is out of range");
        }

        return value;
    }

    private static void RejectExtra(Section section, string[] allowed)
    {
        foreach (KeyValuePair<string, (string Literal, int Line)> pair in section.Values)
        {
            if (Array.IndexOf(allowed, pair.Key) < 0)
            {
                throw FlowTextException.AtUnknownKey(pair.Value.Line, allowed);
            }
        }
    }

    private static FlowVersionStatus ParseStatus(string raw, int line) => raw switch
    {
        "draft" => FlowVersionStatus.Draft,
        "published" => FlowVersionStatus.Published,
        "retired" => FlowVersionStatus.Retired,
        _ => throw FlowTextException.At(line, "unknown status"),
    };

    private static List<Section> Split(string text, ProcessTextLimits limits)
    {
        Section root = new(SectionKind.Root, 1);
        Section current = root;
        List<Section> sections = [root];
        int nodes = 0;
        int edges = 0;
        int rules = 0;
        int position = 0;
        int lineNumber = 0;
        while (position < text.Length)
        {
            // Lines are cut one at a time, so the length is known before the line is copied.
            int found = text.AsSpan(position).IndexOfAny('\n', '\r');
            int end = found < 0 ? text.Length : position + found;
            lineNumber++;
            if (lineNumber > limits.MaxLines)
            {
                throw FlowTextException.AtLimit(lineNumber, "the document has more lines", ProcessTextLimits.Describe("line count", limits.MaxLines));
            }

            if (end - position > limits.MaxLineLength)
            {
                throw FlowTextException.AtLimit(lineNumber, "the line is longer", ProcessTextLimits.Describe("line length", limits.MaxLineLength));
            }

            string line = text.Substring(position, end - position).Trim();
            position = end;
            if (position < text.Length)
            {
                position += text[position] == '\r' && position + 1 < text.Length && text[position + 1] == '\n' ? 2 : 1;
            }

            if (line.Length == 0 || line[0] == '#')
            {
                continue;
            }

            if (line.StartsWith("[[", StringComparison.Ordinal))
            {
                if (!line.EndsWith("]]", StringComparison.Ordinal))
                {
                    throw FlowTextException.At(lineNumber, "a section header must end with `]]`");
                }

                string name = line[2..^2].Trim();
                if ((name == "nodes" && ++nodes > limits.MaxNodes)
                    || (name == "edges" && ++edges > limits.MaxEdges)
                    || (name == "rules" && ++rules > limits.MaxRules))
                {
                    int limit = name == "nodes" ? limits.MaxNodes : name == "edges" ? limits.MaxEdges : limits.MaxRules;
                    throw FlowTextException.AtLimit(lineNumber, name == "nodes" ? "the document has more nodes" : name == "edges" ? "the document has more edges" : "the document has more rules", ProcessTextLimits.Describe("section count", limit));
                }

                current = name switch
                {
                    "nodes" => new Section(SectionKind.Node, lineNumber),
                    "edges" => new Section(SectionKind.Edge, lineNumber),
                    "rules" => new Section(SectionKind.Rule, lineNumber),
                    _ => new Section(SectionKind.Unknown, lineNumber),
                };
                sections.Add(current);
                continue;
            }

            if (line[0] == '[')
            {
                if (!line.EndsWith(']'))
                {
                    throw FlowTextException.At(lineNumber, "a table header must end with `]`");
                }

                current = new Section(SectionKind.Unknown, lineNumber);
                sections.Add(current);
                continue;
            }

            int equals = line.IndexOf('=', StringComparison.Ordinal);
            if (equals <= 0)
            {
                throw FlowTextException.At(lineNumber, "expected `key = value`");
            }

            string key = line[..equals].Trim();
            string literal = line[(equals + 1)..].Trim();
            if (key.Length == 0 || literal.Length == 0)
            {
                throw FlowTextException.At(lineNumber, "expected `key = value`");
            }

            if (key.Length > limits.MaxKeyLength)
            {
                throw FlowTextException.AtLimit(lineNumber, "a key is longer", ProcessTextLimits.Describe("key length", limits.MaxKeyLength));
            }

            if (literal.Length > limits.MaxValueLength)
            {
                throw FlowTextException.AtLimit(lineNumber, "a value is longer", ProcessTextLimits.Describe("value length", limits.MaxValueLength));
            }

            if (!current.Values.TryAdd(key, (literal, lineNumber)))
            {
                throw FlowTextException.AtKey(lineNumber, key, "appears twice");
            }
        }

        return sections;
    }

    private enum SectionKind
    {
        Root,
        Node,
        Edge,
        Rule,
        Unknown,
    }

    private sealed class Section
    {
        public Section(SectionKind kind, int line)
        {
            Kind = kind;
            Line = line;
        }

        public SectionKind Kind { get; }

        public int Line { get; }

        public int JoinCount { get; set; } = 1;

        public Dictionary<string, (string Literal, int Line)> Values { get; } = new(StringComparer.Ordinal);

        public bool Has(string key) => Values.ContainsKey(key);

        public int LineOf(string key) => Values.TryGetValue(key, out (string Literal, int Line) value) ? value.Line : Line;
    }
}
