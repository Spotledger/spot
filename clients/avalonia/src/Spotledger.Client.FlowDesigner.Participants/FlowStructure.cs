using System;
using System.Collections.Generic;
using System.Linq;

namespace Spotledger.Client.FlowDesigner.Participants;

/// What a backward edge does to the effects already produced (ADR-080 section 6.5). The edge's
/// label is the marker, exactly as the server reads it: <c>undo</c> goes back and reverses the
/// effects, <c>rewind</c> goes back and keeps them (the next pass revises the records). A forward
/// edge is <see cref="Forward"/>.
public enum EdgeMode
{
    Forward,
    Rewind,
    Undo,
}

/// Reads and writes the mode of an edge, and finds the backward edges of a document.
public static class EdgeModes
{

    public const string UndoLabel = "undo";

    public const string RewindLabel = "rewind";

    public static EdgeMode Of(FlowEdge edge)
    {
        ArgumentNullException.ThrowIfNull(edge);
        return edge.Label switch
        {
            UndoLabel => EdgeMode.Undo,
            RewindLabel => EdgeMode.Rewind,
            _ => EdgeMode.Forward,
        };
    }

    public static string Word(this EdgeMode mode) => mode switch
    {
        EdgeMode.Undo => "Undo",
        EdgeMode.Rewind => "Rewind",
        _ => "Forward",
    };

    /// The command that types a backward edge. The edge keeps no other label while it is typed, so a
    /// label that is not a mode (a person's own words) is refused rather than overwritten; null when
    /// the edge is not there or its label is in the way.
    public static IFlowCommand? SetMode(FlowDocument document, FlowEdgeKey key, EdgeMode mode)
    {
        ArgumentNullException.ThrowIfNull(document);
        FlowEdge? edge = document.Edges.FirstOrDefault(candidate => candidate.Key == key);
        if (edge is null)
        {
            return null;
        }

        bool otherLabel = !string.IsNullOrEmpty(edge.Label) && Of(edge) == EdgeMode.Forward;
        if (otherLabel)
        {
            return null;
        }

        string? label = mode switch
        {
            EdgeMode.Undo => UndoLabel,
            EdgeMode.Rewind => RewindLabel,
            _ => null,
        };
        return new SetEdgeFieldCommand(key, "label", label);
    }

    /// The edges that go back to a step already on the path: found by a depth-first walk from the
    /// entry steps (those with no incoming edge, or the first step when there is none). Deterministic:
    /// definition order decides every tie.
    public static IReadOnlyList<FlowEdge> Backward(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        Dictionary<string, List<FlowEdge>> outgoing = new(StringComparer.Ordinal);
        foreach (FlowEdge edge in document.Edges)
        {
            if (!outgoing.TryGetValue(edge.From, out List<FlowEdge>? list))
            {
                list = [];
                outgoing[edge.From] = list;
            }

            list.Add(edge);
        }

        HashSet<string> hasIncoming = new(document.Edges.Select(edge => edge.To), StringComparer.Ordinal);
        List<string> entries = [.. document.Nodes.Where(node => !hasIncoming.Contains(node.Id)).Select(node => node.Id)];
        if (entries.Count == 0 && document.Nodes.Count > 0)
        {
            entries.Add(document.Nodes[0].Id);
        }

        // Steps that no entry reaches (a cycle cut off from the start) are walked last, in order.
        entries.AddRange(document.Nodes.Select(node => node.Id));
        HashSet<string> visited = new(StringComparer.Ordinal);
        HashSet<string> onPath = new(StringComparer.Ordinal);
        List<FlowEdge> back = [];
        foreach (string entry in entries)
        {
            if (visited.Contains(entry))
            {
                continue;
            }

            Walk(entry, outgoing, visited, onPath, back);
        }

        return back;
    }

    private static void Walk(
        string start,
        Dictionary<string, List<FlowEdge>> outgoing,
        HashSet<string> visited,
        HashSet<string> onPath,
        List<FlowEdge> back)
    {
        // An explicit stack, so a deep process cannot overflow the call stack.
        Stack<(string Node, int Next)> stack = new();
        visited.Add(start);
        onPath.Add(start);
        stack.Push((start, 0));
        while (stack.Count > 0)
        {
            (string node, int next) = stack.Pop();
            List<FlowEdge> edges = outgoing.GetValueOrDefault(node) ?? [];
            if (next >= edges.Count)
            {
                onPath.Remove(node);
                continue;
            }

            stack.Push((node, next + 1));
            FlowEdge edge = edges[next];
            if (onPath.Contains(edge.To))
            {
                back.Add(edge);
            }
            else if (visited.Add(edge.To))
            {
                onPath.Add(edge.To);
                stack.Push((edge.To, 0));
            }
        }
    }

    /// The designer's own hints about the modes: a backward edge nobody typed (the server then reads
    /// it as forward and sees a loop), and a mode on an edge that does not go back.
    public static IReadOnlyList<FlowProblem> Hints(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        HashSet<FlowEdgeKey> back = [.. Backward(document).Select(edge => edge.Key)];
        List<FlowProblem> hints = [];
        foreach (FlowEdge edge in document.Edges)
        {
            EdgeMode mode = Of(edge);
            bool goesBack = back.Contains(edge.Key);
            if (goesBack && mode == EdgeMode.Forward && !string.Equals(edge.Label, ParticipantKinds.TimeoutEdgeLabel, StringComparison.Ordinal))
            {
                hints.Add(new FlowProblem(
                    "back_edge_untyped",
                    ParticipantStrings.BackEdgeUntyped(edge.From, edge.To),
                    FlowProblemSeverity.Hint,
                    EdgeFrom: edge.From,
                    EdgeTo: edge.To));
            }
            else if (!goesBack && mode != EdgeMode.Forward)
            {
                hints.Add(new FlowProblem(
                    "mode_on_forward_edge",
                    ParticipantStrings.ModeOnForwardEdge(edge.From, edge.To, mode),
                    FlowProblemSeverity.Hint,
                    EdgeFrom: edge.From,
                    EdgeTo: edge.To));
            }
        }

        return hints;
    }
}

/// The held regions of a document (ADR-080 section 6.7): the region of a commit point is every
/// step from which it can be reached over forward edges without passing through another commit
/// point. The commit point itself and everything after the last one are outside. A process with no
/// commit point holds nothing. The server decides what is held; the canvas shades this region so a
/// person sees where the hold begins.
public static class CommitRegions
{

    public static IReadOnlyDictionary<string, IReadOnlySet<string>> Of(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        Dictionary<string, List<string>> predecessors = new(StringComparer.Ordinal);
        foreach (FlowEdge edge in document.Edges)
        {
            if (EdgeModes.Of(edge) != EdgeMode.Forward)
            {
                continue;
            }

            if (!predecessors.TryGetValue(edge.To, out List<string>? list))
            {
                list = [];
                predecessors[edge.To] = list;
            }

            list.Add(edge.From);
        }

        HashSet<string> commits = new(
            document.Nodes.Where(node => string.Equals(node.Kind, ParticipantKinds.CommitPoint, StringComparison.Ordinal)).Select(node => node.Id),
            StringComparer.Ordinal);
        Dictionary<string, IReadOnlySet<string>> regions = new(StringComparer.Ordinal);
        foreach (FlowNode node in document.Nodes.Where(node => commits.Contains(node.Id)))
        {
            HashSet<string> region = new(StringComparer.Ordinal);
            Queue<string> queue = new([node.Id]);
            while (queue.Count > 0)
            {
                string current = queue.Dequeue();
                foreach (string before in predecessors.GetValueOrDefault(current) ?? [])
                {
                    if (commits.Contains(before) || !region.Add(before))
                    {
                        continue;
                    }

                    queue.Enqueue(before);
                }
            }

            regions[node.Id] = region;
        }

        return regions;
    }

    /// Every step that lies in some commit point's region.
    public static IReadOnlySet<string> Held(FlowDocument document) =>
        new HashSet<string>(Of(document).Values.SelectMany(region => region), StringComparer.Ordinal);
}
