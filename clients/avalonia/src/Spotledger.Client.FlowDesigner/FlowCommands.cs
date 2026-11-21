using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Spotledger.Client.FlowDesigner;

/// One undoable edit of the document: it knows how to apply itself and how to reverse it.
public interface IFlowCommand
{
    string Label { get; }
    void Apply(FlowDocument document);
    void Undo(FlowDocument document);
}

/// One entry handed to the workspace undo stack (WP-UI-17). The shell owns the stack, so a
/// designer edit joins the same undo history as every other workspace edit; this project only
/// hands it an entry. The shape mirrors the seam <c>Spotledger.Client.Graph</c> already uses, so a
/// host adapter can bind both to one stack.
public sealed record FlowUndoEntry(string Label, Func<CancellationToken, Task> Undo);

/// The workspace undo seam the designer reports to, when the host supplies one.
public interface IFlowWorkspaceUndo
{
    void Record(FlowUndoEntry entry);
}

/// The join, terminal, error and retry settings of one node, captured for an undo.
public sealed record FlowNodeSettings(
    FlowJoinKind Join,
    int JoinCount,
    bool Terminal,
    string? Error,
    ulong? TimeoutSecs,
    int RetryAttempts,
    ulong RetryDelaySecs)
{

    public static FlowNodeSettings Of(FlowNode node)
    {
        ArgumentNullException.ThrowIfNull(node);
        return new FlowNodeSettings(
            node.Join,
            node.JoinCount,
            node.Terminal,
            node.Error,
            node.TimeoutSecs,
            node.RetryAttempts,
            node.RetryDelaySecs);
    }

    public void ApplyTo(FlowNode node)
    {
        ArgumentNullException.ThrowIfNull(node);
        node.Join = Join;
        node.JoinCount = JoinCount;
        node.Terminal = Terminal;
        node.Error = Error;
        node.TimeoutSecs = TimeoutSecs;
        node.RetryAttempts = RetryAttempts;
        node.RetryDelaySecs = RetryDelaySecs;
    }
}

/// The document's command stack: every edit is a command with its own inverse, so undo and redo
/// are exact and deterministic and the server's text form changes only where the user meant it.
public sealed class FlowEditor
{

    /// The history depth when none is given.
    public const int DefaultMaxHistory = 500;

    private readonly BoundedStack _undo;

    private readonly BoundedStack _redo;

    public FlowEditor(FlowDocument document, int maxHistory = DefaultMaxHistory)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentOutOfRangeException.ThrowIfLessThan(maxHistory, 1);
        Document = document;
        MaxHistory = maxHistory;
        _undo = new BoundedStack(maxHistory);
        _redo = new BoundedStack(maxHistory);
    }

    /// The most undo steps (and, separately, redo steps) kept; the oldest is dropped first.
    public int MaxHistory { get; }

    public FlowDocument Document { get; }

    public IFlowWorkspaceUndo? WorkspaceUndo { get; set; }

    public bool CanEdit { get; set; } = true;

    public event EventHandler? Changed;

    public bool CanUndo => _undo.Count > 0;

    public bool CanRedo => _redo.Count > 0;

    public string? UndoLabel => _undo.Count > 0 ? _undo.Peek().Label : null;

    public string? RedoLabel => _redo.Count > 0 ? _redo.Peek().Label : null;

    public int UndoDepth => _undo.Count;

    public int RedoDepth => _redo.Count;

    public bool Execute(IFlowCommand command)
    {
        ArgumentNullException.ThrowIfNull(command);
        if (!CanEdit)
        {
            return false;
        }

        command.Apply(Document);
        _undo.Push(command);
        _redo.Clear();
        RecordOnWorkspace(command);
        Changed?.Invoke(this, EventArgs.Empty);
        return true;
    }

    public bool Undo()
    {
        if (!CanEdit || _undo.Count == 0)
        {
            return false;
        }

        IFlowCommand command = _undo.Pop();
        command.Undo(Document);
        _redo.Push(command);
        Changed?.Invoke(this, EventArgs.Empty);
        return true;
    }

    public bool Redo()
    {
        if (!CanEdit || _redo.Count == 0)
        {
            return false;
        }

        IFlowCommand command = _redo.Pop();
        command.Apply(Document);
        _undo.Push(command);
        Changed?.Invoke(this, EventArgs.Empty);
        return true;
    }

    public void ClearHistory()
    {
        _undo.Clear();
        _redo.Clear();
    }

    private void RecordOnWorkspace(IFlowCommand command)
    {
        if (WorkspaceUndo is null)
        {
            return;
        }

        WorkspaceUndo.Record(new FlowUndoEntry(
            command.Label,
            cancellationToken =>
            {
                // The inverse runs through the same read-only check as any other edit.
                if (!CanEdit)
                {
                    throw new InvalidOperationException(FlowStrings.RefusedReadOnly);
                }

                command.Undo(Document);
                Changed?.Invoke(this, EventArgs.Empty);
                return Task.CompletedTask;
            }));
    }
}

/// A stack that drops from the bottom once it holds <c>capacity</c> items.
internal sealed class BoundedStack
{
    private readonly LinkedList<IFlowCommand> _items = new();

    private readonly int _capacity;

    public BoundedStack(int capacity) => _capacity = capacity;

    public int Count => _items.Count;

    public void Push(IFlowCommand command)
    {
        _items.AddLast(command);
        if (_items.Count > _capacity)
        {
            _items.RemoveFirst();
        }
    }

    public IFlowCommand Pop()
    {
        IFlowCommand last = _items.Last!.Value;
        _items.RemoveLast();
        return last;
    }

    public IFlowCommand Peek() => _items.Last!.Value;

    public void Clear() => _items.Clear();
}

/// Adds one node at a definition index (the end when the index is past the end).
public sealed class AddNodeCommand : IFlowCommand
{

    private readonly FlowNode _node;

    private readonly int _index;

    public AddNodeCommand(FlowNode node, int index = -1)
    {
        ArgumentNullException.ThrowIfNull(node);
        _node = node.Clone();
        _index = index;
    }

    public string Label => "Add " + _node.Kind + " node " + _node.Id;

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        document.InsertNode(_index < 0 ? document.Nodes.Count : _index, _node.Clone());
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        int index = document.IndexOfNode(_node.Id);
        if (index >= 0)
        {
            document.RemoveNodeAt(index);
        }
    }
}

/// Deletes nodes (with every edge that touches them) and the edges the user picked explicitly, as
/// one undoable step, restoring definition order on undo.
public sealed class DeleteSelectionCommand : IFlowCommand
{

    private readonly List<string> _nodeIds;

    private readonly List<FlowEdgeKey> _edgeKeys;

    private readonly List<(int Index, FlowNode Node)> _removedNodes = [];

    private readonly List<(int Index, FlowEdge Edge)> _removedEdges = [];

    public DeleteSelectionCommand(IEnumerable<string> nodeIds, IEnumerable<FlowEdgeKey> edgeKeys)
    {
        ArgumentNullException.ThrowIfNull(nodeIds);
        ArgumentNullException.ThrowIfNull(edgeKeys);
        _nodeIds = [.. nodeIds];
        _edgeKeys = [.. edgeKeys];
    }

    public string Label => _nodeIds.Count == 0 ? "Delete connection" : "Delete " + FlowStrings.Selected(_nodeIds.Count);

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        _removedNodes.Clear();
        _removedEdges.Clear();

        HashSet<string> doomed = new(_nodeIds, StringComparer.Ordinal);
        for (int i = document.Edges.Count - 1; i >= 0; i--)
        {
            FlowEdge edge = document.Edges[i];
            bool explicitEdge = _edgeKeys.Any(key => key.From == edge.From && key.To == edge.To);
            if (explicitEdge || doomed.Contains(edge.From) || doomed.Contains(edge.To))
            {
                _removedEdges.Add((i, edge));
                document.RemoveEdge(edge);
            }
        }

        for (int i = document.Nodes.Count - 1; i >= 0; i--)
        {
            FlowNode node = document.Nodes[i];
            if (doomed.Contains(node.Id))
            {
                _removedNodes.Add((i, node));
                document.RemoveNodeAt(i);
            }
        }
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        foreach ((int index, FlowNode node) in _removedNodes.OrderBy(pair => pair.Index))
        {
            document.InsertNode(index, node);
        }

        foreach ((int index, FlowEdge edge) in _removedEdges.OrderBy(pair => pair.Index))
        {
            document.InsertEdge(index, edge);
        }
    }
}

/// Adds one edge at the end of the edge list.
public sealed class AddEdgeCommand : IFlowCommand
{

    private readonly FlowEdge _edge;

    public AddEdgeCommand(FlowEdge edge)
    {
        ArgumentNullException.ThrowIfNull(edge);
        _edge = edge.Clone();
    }

    public string Label => "Connect " + _edge.From + " to " + _edge.To;

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        document.AddEdge(_edge.Clone());
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        FlowEdge? edge = document.Edges.FirstOrDefault(candidate => candidate.Key == _edge.Key);
        if (edge is not null)
        {
            document.RemoveEdge(edge);
        }
    }
}

/// Moves the selected nodes by a grid-snapped offset, as one undoable step.
public sealed class MoveNodesCommand : IFlowCommand
{

    private readonly Dictionary<string, (FlowPoint Before, FlowPoint After)> _moves = new(StringComparer.Ordinal);

    public MoveNodesCommand(FlowDocument document, IEnumerable<string> nodeIds, double dx, double dy)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(nodeIds);
        foreach (string id in nodeIds)
        {
            FlowNode? node = document.Node(id);
            if (node is null)
            {
                continue;
            }

            _moves[id] = (node.Position, node.Position.Offset(dx, dy));
        }
    }

    public string Label => "Move " + FlowStrings.Selected(_moves.Count);

    public bool HasMovement => _moves.Count > 0 && _moves.Values.Any(pair => pair.Before != pair.After);

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        foreach (KeyValuePair<string, (FlowPoint Before, FlowPoint After)> pair in _moves)
        {
            FlowNode? node = document.Node(pair.Key);
            if (node is not null)
            {
                node.Position = pair.Value.After;
            }
        }
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        foreach (KeyValuePair<string, (FlowPoint Before, FlowPoint After)> pair in _moves)
        {
            FlowNode? node = document.Node(pair.Key);
            if (node is not null)
            {
                node.Position = pair.Value.Before;
            }
        }
    }
}

/// Sets (or removes, when the value is null) one config key of a node.
public sealed class SetConfigCommand : IFlowCommand
{

    private readonly string _nodeId;

    private readonly string _key;

    private readonly FlowValue? _value;

    private bool _hadBefore;

    private FlowValue? _before;

    public SetConfigCommand(string nodeId, string key, FlowValue? value)
    {
        _nodeId = FlowId.Require(nodeId, nameof(nodeId));
        ArgumentNullException.ThrowIfNull(key);
        _key = key;
        _value = value;
    }

    public string Label => "Set " + _key + " on " + _nodeId;

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        FlowNode? node = document.Node(_nodeId);
        if (node is null)
        {
            return;
        }

        _hadBefore = node.Map.TryGetValue(_key, out FlowValue? before);
        _before = _hadBefore ? before : null;
        if (_value is null)
        {
            node.Map.Remove(_key);
        }
        else
        {
            node.Map[_key] = _value;
        }
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        FlowNode? node = document.Node(_nodeId);
        if (node is null)
        {
            return;
        }

        if (_hadBefore && _before is { } before)
        {
            node.Map[_key] = before;
        }
        else
        {
            node.Map.Remove(_key);
        }
    }
}

/// Sets the join, terminal, error, timeout and retry settings of a node.
public sealed class SetNodeSettingsCommand : IFlowCommand
{

    private readonly string _nodeId;

    private readonly FlowNodeSettings _after;

    private FlowNodeSettings? _before;

    public SetNodeSettingsCommand(string nodeId, FlowNodeSettings after)
    {
        _nodeId = FlowId.Require(nodeId, nameof(nodeId));
        ArgumentNullException.ThrowIfNull(after);
        _after = after;
    }

    public string Label => "Change the settings of " + _nodeId;

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        FlowNode? node = document.Node(_nodeId);
        if (node is null)
        {
            return;
        }

        _before = FlowNodeSettings.Of(node);
        _after.ApplyTo(node);
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        if (_before is { } before && document.Node(_nodeId) is { } node)
        {
            before.ApplyTo(node);
        }
    }
}

/// Sets one field of an edge (its condition, rule, label or default flag).
public sealed class SetEdgeFieldCommand : IFlowCommand
{

    private readonly FlowEdgeKey _key;

    private readonly string _field;

    private readonly object? _after;

    private object? _before;

    public SetEdgeFieldCommand(FlowEdgeKey key, string field, object? value)
    {
        ArgumentNullException.ThrowIfNull(field);
        if (field is not ("condition" or "rule" or "label" or "default"))
        {
            throw new ArgumentException("Unknown edge field: " + field, nameof(field));
        }

        _key = key;
        _field = field;
        _after = value;
    }

    public string Label => "Change the " + _field + " of " + _key.From + " to " + _key.To;

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        FlowEdge? edge = document.Edges.FirstOrDefault(candidate => candidate.Key == _key);
        if (edge is null)
        {
            return;
        }

        _before = Read(edge);
        Write(edge, _after);
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        FlowEdge? edge = document.Edges.FirstOrDefault(candidate => candidate.Key == _key);
        if (edge is not null)
        {
            Write(edge, _before);
        }
    }

    private object? Read(FlowEdge edge) => _field switch
    {
        "condition" => edge.Condition,
        "rule" => edge.Rule,
        "label" => edge.Label,
        "default" => edge.Default,
        _ => null,
    };

    private void Write(FlowEdge edge, object? value)
    {
        switch (_field)
        {
            case "condition":
                edge.Condition = value as string;
                break;
            case "rule":
                edge.Rule = value as string;
                break;
            case "label":
                edge.Label = value as string;
                break;
            default:
                edge.Default = value is true;
                break;
        }
    }
}

/// Sets the process title.
public sealed class SetTitleCommand : IFlowCommand
{

    private readonly string? _after;

    private string? _before;

    public SetTitleCommand(string? title) => _after = title;

    public string Label => "Change the title";

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        _before = document.Title;
        document.Title = _after;
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        document.Title = _before;
    }
}
