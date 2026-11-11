using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;

namespace Spotledger.Client.FlowDesigner;

/// Raised when process text is not a process definition this build can read.
///
/// A message never echoes input: a process document can come from a file a user was sent,
/// and the message appears in the UI and in logs. It carries the line number and the kind of fault,
/// and may name keys the FORMAT defines, never a key or value the document supplied. Raise it only
/// through <see cref="At"/>, <see cref="AtKey"/>, <see cref="AtLimit"/> or <see cref="AtUnknownKey"/>,
/// which take the kind as a fixed phrase; a source-guard test refuses concatenation at an error site.
public sealed class FlowTextException : Exception
{
    private static readonly HashSet<string> FormatKeys = new(StringComparer.Ordinal)
    {
        "name", "app", "title", "version", "status",
        "id", "kind", "join", "join_count", "terminal", "error", "timeout_secs", "retry_attempts", "retry_delay_secs",
        "from", "to", "condition", "rule", "label", "default", "expression",
    };

    public FlowTextException(string message, int line = 0)
        : base(line > 0
            ? "line " + line.ToString(CultureInfo.InvariantCulture) + ": " + message
            : message)
    {
        Problem = message;
        Line = line;
    }

    public FlowTextException()
        : this("The process text is not valid.", 0)
    {
    }

    public FlowTextException(string message)
        : this(message, 0)
    {
    }

    public FlowTextException(string message, Exception innerException)
        : base(message, innerException)
    {
        Problem = message;
    }

    public string Problem { get; }

    public int Line { get; }

    /// A fault of a fixed kind at a line (0 for none). The kind must be a fixed phrase, never input.
    public static FlowTextException At(int line, string kind) => new(kind, line);

    /// "`key` kind" when the key is one the format defines, otherwise "a key kind": the document's own
    /// key is never echoed.
    public static FlowTextException AtKey(int line, string key, string kind) =>
        new((FormatKeys.Contains(key) ? "`" + key + "`" : "a key") + " " + kind, line);

    /// "subject than the limit": the subject is a fixed phrase, the limit text comes from the limits.
    public static FlowTextException AtLimit(int line, string subject, string limit) =>
        new(subject + " than the " + limit, line);

    /// An unknown key, listing the keys the format allows there but not the offending one.
    public static FlowTextException AtUnknownKey(int line, IEnumerable<string> allowed) =>
        new("unknown key; the keys allowed here are " + string.Join(", ", allowed.Select(key => "`" + key + "`")), line);
}
