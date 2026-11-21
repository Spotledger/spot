using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace Spotledger.Client.FlowDesigner;

/// Writes the canonical process text form: the same bytes the server's <c>spot-bpm-model</c>
/// writer produces, so a draft saved from the designer hashes identically to one written
/// anywhere else. Keys are sorted, one scalar per line, the join and terminal/error/timeout/retry
/// fields in the server's order, and the config table after them.
public static class ProcessTextWriter
{

    public const string Header =
        "# Spotledger process definition (spot-bpm-model). Canonical: keys sorted, one scalar per line.";

    private static readonly string[] CommonKeys =
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

    public static string Write(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        StringBuilder builder = new();
        builder.Append(Header).Append('\n');
        Line(builder, "name", ProcessText.Quote(document.Name));
        Line(builder, "app", ProcessText.Quote(document.App));
        if (document.Title is { } title)
        {
            Line(builder, "title", ProcessText.Quote(title));
        }

        Line(builder, "version", document.Version.ToString(CultureInfo.InvariantCulture));
        Line(builder, "status", Token(document.Status));

        foreach (FlowNode node in document.Nodes)
        {
            WriteNode(builder, node);
        }

        foreach (FlowEdge edge in document.Edges)
        {
            builder.Append('\n').Append("[[edges]]").Append('\n');
            Line(builder, "from", ProcessText.Quote(edge.From));
            Line(builder, "to", ProcessText.Quote(edge.To));
            if (edge.Condition is { } condition)
            {
                Line(builder, "condition", ProcessText.Quote(condition));
            }

            if (edge.Rule is { } rule)
            {
                Line(builder, "rule", ProcessText.Quote(rule));
            }

            if (edge.Label is { } label)
            {
                Line(builder, "label", ProcessText.Quote(label));
            }

            if (edge.Default)
            {
                Line(builder, "default", "true");
            }
        }

        foreach (FlowRule rule in document.Rules)
        {
            builder.Append('\n').Append("[[rules]]").Append('\n');
            Line(builder, "id", ProcessText.Quote(rule.Id));
            Line(builder, "expression", ProcessText.Quote(rule.Expression));
        }

        return builder.ToString();
    }

    public static string Token(FlowVersionStatus status) => status switch
    {
        FlowVersionStatus.Draft => "\"draft\"",
        FlowVersionStatus.Published => "\"published\"",
        FlowVersionStatus.Retired => "\"retired\"",
        _ => throw new ArgumentOutOfRangeException(nameof(status)),
    };

    private static void WriteNode(StringBuilder builder, FlowNode node)
    {
        builder.Append('\n').Append("[[nodes]]").Append('\n');
        Line(builder, "id", ProcessText.Quote(node.Id));
        Line(builder, "kind", ProcessText.Quote(node.Kind));
        switch (node.Join)
        {
            case FlowJoinKind.All:
                break;
            case FlowJoinKind.Any:
                Line(builder, "join", "\"any\"");
                break;
            case FlowJoinKind.Quorum:
                Line(builder, "join", "\"quorum\"");
                Line(builder, "join_count", node.JoinCount.ToString(CultureInfo.InvariantCulture));
                break;
            default:
                throw new ArgumentOutOfRangeException(nameof(node));
        }

        if (node.Terminal)
        {
            Line(builder, "terminal", "true");
        }

        if (node.Error is { } error)
        {
            Line(builder, "error", ProcessText.Quote(error));
        }

        if (node.TimeoutSecs is { } timeout)
        {
            Line(builder, "timeout_secs", timeout.ToString(CultureInfo.InvariantCulture));
        }

        if (node.RetryAttempts > 1 || node.RetryDelaySecs > 0)
        {
            Line(builder, "retry_attempts", node.RetryAttempts.ToString(CultureInfo.InvariantCulture));
            if (node.RetryDelaySecs > 0)
            {
                Line(builder, "retry_delay_secs", node.RetryDelaySecs.ToString(CultureInfo.InvariantCulture));
            }
        }

        List<string> keys = new(node.Config.Count);
        foreach (string key in node.Config.Keys)
        {
            if (Array.IndexOf(CommonKeys, key) < 0)
            {
                keys.Add(key);
            }
        }

        keys.Sort(StringComparer.Ordinal);
        foreach (string key in keys)
        {
            Line(builder, key, node.Config[key].Render());
        }
    }

    private static void Line(StringBuilder builder, string key, string value)
    {
        builder.Append(key).Append(" = ").Append(value).Append('\n');
    }
}
