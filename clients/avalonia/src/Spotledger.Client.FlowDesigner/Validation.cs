using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Spotledger.Client.FlowDesigner;

/// Debounced, generation-checked validation against the server.
///
/// Time is a parameter, never the wall clock: the host calls <see cref="MarkEdited"/> with the
/// instant of the edit and <see cref="PollAsync"/> on its tick, so the debounce window is exact and
/// a test needs no sleeping. Every request carries the generation it was sent for, and a result
/// from an older generation is dropped, so a slow answer can never overwrite a newer one. The
/// server's problems are the ones the canvas shows; <see cref="LocalHints"/> adds the client's own
/// advice as hints, which never become errors.
public sealed class FlowValidationCoordinator
{

    private readonly IFlowDesignerServer _server;

    private readonly FlowEditor _editor;

    private readonly List<FlowProblem> _problems = [];

    private TimeSpan _lastEdit;

    private int _generation;

    private int _sentGeneration = -1;

    private bool _dirty;

    public FlowValidationCoordinator(IFlowDesignerServer server, FlowEditor editor, TimeSpan debounce)
    {
        ArgumentNullException.ThrowIfNull(server);
        ArgumentNullException.ThrowIfNull(editor);
        ArgumentOutOfRangeException.ThrowIfLessThan(debounce, TimeSpan.Zero);

        _server = server;
        _editor = editor;
        Debounce = debounce;
    }

    public TimeSpan Debounce { get; }

    public IReadOnlyList<FlowProblem> Problems => _problems;

    public bool IsChecking { get; private set; }

    public bool IsDirty => _dirty;

    public int AcceptedRevision { get; private set; } = -1;

    public int SentCount { get; private set; }

    public event EventHandler? ProblemsChanged;

    public void MarkEdited(TimeSpan now)
    {
        _generation++;
        _lastEdit = now;
        _dirty = true;
    }

    public bool IsDue(TimeSpan now) => _dirty && !IsChecking && now - _lastEdit >= Debounce;

    public async Task<bool> PollAsync(TimeSpan now, CancellationToken cancellationToken)
    {
        if (!IsDue(now))
        {
            return false;
        }

        await ValidateNowAsync(cancellationToken).ConfigureAwait(true);
        return true;
    }

    public async Task<IReadOnlyList<FlowProblem>> ValidateNowAsync(CancellationToken cancellationToken)
    {
        int generation = _generation;
        string text = _editor.Document.ToText();
        _dirty = false;
        _sentGeneration = generation;
        IsChecking = true;
        ProblemsChanged?.Invoke(this, EventArgs.Empty);
        FlowValidationReport report;
        try
        {
            report = await _server.ValidateAsync(text, cancellationToken).ConfigureAwait(true);
        }
        catch (OperationCanceledException)
        {
            IsChecking = false;
            ProblemsChanged?.Invoke(this, EventArgs.Empty);
            throw;
        }
        finally
        {
            IsChecking = false;
        }

        SentCount++;
        if (generation != _generation || _sentGeneration != generation)
        {
            // A newer edit landed while this check was in flight: its answer is stale.
            ProblemsChanged?.Invoke(this, EventArgs.Empty);
            return [];
        }

        AcceptedRevision = report.Revision;
        _problems.Clear();
        _problems.AddRange(report.Problems);
        _problems.AddRange(LocalHints.Check(_editor.Document));
        ProblemsChanged?.Invoke(this, EventArgs.Empty);
        return _problems;
    }

    public IReadOnlyList<FlowProblem> ForNode(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        return [.. _problems.Where(problem => string.Equals(problem.NodeId, nodeId, StringComparison.Ordinal))];
    }

    public FlowProblem? WorstFor(string nodeId) =>
        ForNode(nodeId)
            .OrderBy(problem => problem.Severity)
            .FirstOrDefault();
}

/// The client's own advice about a document, marked as hints. It never rejects anything and never
/// blocks a save: the server decides, and these only help an analyst notice the obvious. Each hint
/// carries a code so a host can switch it off individually.
public static class LocalHints
{

    public static IReadOnlyList<FlowProblem> Check(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        List<FlowProblem> hints = [];

        if (document.Nodes.Count == 0)
        {
            return hints;
        }

        foreach (FlowNode node in document.Nodes)
        {
            if (document.Incoming(node.Id).Count == 0 && document.Outgoing(node.Id).Count == 0 && document.Nodes.Count > 1)
            {
                hints.Add(new FlowProblem(
                    "hint.unconnected",
                    "The node " + node.Id + " has no connections.",
                    FlowProblemSeverity.Hint,
                    node.Id));
            }
        }

        if (!document.Nodes.Any(node => string.Equals(node.Kind, "trigger", StringComparison.Ordinal)))
        {
            hints.Add(new FlowProblem(
                "hint.no_trigger",
                "The process has no trigger, so nothing starts it.",
                FlowProblemSeverity.Hint));
        }

        foreach (string id in CycleNodes(document))
        {
            hints.Add(new FlowProblem(
                "hint.cycle",
                "The node " + id + " sits on a loop; the server decides whether that loop is allowed.",
                FlowProblemSeverity.Hint,
                id));
        }

        return hints;
    }

    private static List<string> CycleNodes(FlowDocument document)
    {
        // Iterative on purpose: a long chain must not exhaust the stack, and one adjacency pass
        // keeps the walk linear in the size of the graph.
        Dictionary<string, List<string>> next = new(StringComparer.Ordinal);
        foreach (FlowEdge edge in document.Edges)
        {
            if (!next.TryGetValue(edge.From, out List<string>? outgoing))
            {
                outgoing = [];
                next[edge.From] = outgoing;
            }

            outgoing.Add(edge.To);
        }

        Dictionary<string, int> state = new(StringComparer.Ordinal);
        HashSet<string> cyclic = new(StringComparer.Ordinal);
        foreach (FlowNode node in document.Nodes)
        {
            if (state.ContainsKey(node.Id))
            {
                continue;
            }

            Stack<(string Id, int Index)> stack = new();
            state[node.Id] = 1;
            stack.Push((node.Id, 0));
            while (stack.Count > 0)
            {
                (string id, int index) = stack.Pop();
                if (!next.TryGetValue(id, out List<string>? targets) || index >= targets.Count)
                {
                    state[id] = 2;
                    continue;
                }

                stack.Push((id, index + 1));
                string target = targets[index];
                if (!state.TryGetValue(target, out int seen))
                {
                    state[target] = 1;
                    stack.Push((target, 0));
                }
                else if (seen == 1)
                {
                    cyclic.Add(target);
                }
            }
        }

        List<string> result = [.. cyclic];
        result.Sort(StringComparer.Ordinal);
        return result;
    }
}
