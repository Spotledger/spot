using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>The kinds a composite's expansion produces: the layout primitives and the
/// catalog presentations, plus a placeholder for a control name the catalogue does not have.</summary>
public enum ExpandedNodeKind
{
    /// <summary>A horizontal layout.</summary>
    Row,
    /// <summary>A vertical layout.</summary>
    Column,
    /// <summary>A grid of <see cref="ExpandedNode.Columns"/> columns.</summary>
    Grid,
    /// <summary>A titled group.</summary>
    Group,
    /// <summary>A horizontal rule.</summary>
    Separator,
    /// <summary>Body text.</summary>
    Label,
    /// <summary>A heading.</summary>
    Heading,
    /// <summary>A short status badge.</summary>
    Badge,
    /// <summary>A progress bar at <see cref="ExpandedNode.Percent"/>.</summary>
    Progress,
    /// <summary>A number.</summary>
    Number,
    /// <summary>A money amount.</summary>
    Currency,
    /// <summary>A date.</summary>
    Date,
    /// <summary>A button whose click is an action of the same app.</summary>
    Button,
    /// <summary>A visible stand-in for a control the catalogue does not have.</summary>
    Placeholder
}

/// <summary>One node of an expanded composite: what to draw, never how to draw it.</summary>
public sealed record ExpandedNode
{
    /// <summary>Creates a node of one kind.</summary>
    public ExpandedNode(ExpandedNodeKind kind) => Kind = kind;

    /// <summary>The node's kind.</summary>
    public ExpandedNodeKind Kind { get; }

    /// <summary>The literal text (label, heading, badge, button), or null.</summary>
    public string? Text { get; init; }

    /// <summary>The formatted value (number, currency, date), or null.</summary>
    public string? Value { get; init; }

    /// <summary>The progress percentage in 0..100.</summary>
    public int Percent { get; init; }

    /// <summary>The grid's column count (0 for other layouts).</summary>
    public int Columns { get; init; }

    /// <summary>A layout's or group's title, or null.</summary>
    public string? Title { get; init; }

    /// <summary>The child nodes, in order.</summary>
    public IReadOnlyList<ExpandedNode> Children { get; init; } = [];

    /// <summary>The action a button invokes, or null.</summary>
    public ActionModel? Action { get; init; }

    /// <summary>The control name a placeholder stands in for, or null.</summary>
    public string? Placeholder { get; init; }
}

/// <summary>
/// One composite component expanded with its props: the node tree a renderer draws. The
/// expansion enforces the contract's limits again on the client — depth 8, 500 nodes, a
/// <c>repeat</c> bound — and turns every reference it cannot resolve into a visible
/// placeholder with a diagnostic, never an exception and never a blank.
/// </summary>
public sealed class ComponentModel
{
    private ComponentModel(ComponentSpec component, ExpandedNode? root)
    {
        Component = component;
        Root = root;
    }

    /// <summary>The component that was expanded.</summary>
    public ComponentSpec Component { get; }

    /// <summary>The expanded tree, or null when the expansion was refused whole.</summary>
    public ExpandedNode? Root { get; }

    /// <summary>True when there is a tree to draw.</summary>
    public bool IsUsable => Root is not null;

    /// <summary>
    /// Expands <paramref name="component" /> with <paramref name="props" />. Inside a
    /// component an expression reads the props as <c>doc.&lt;prop&gt;</c>.
    /// </summary>
    /// <param name="component">The component to expand.</param>
    /// <param name="props">The prop values, by name.</param>
    /// <param name="context">The rendering context (locale, instant, the set's diagnostics).</param>
    /// <param name="set">The document the component's <c>use</c> references resolve against.</param>
    /// <param name="actions">Resolves an action id to a live model for a button.</param>
    /// <param name="fill">Slot bodies an instance supplies, already expanded by the caller.</param>
    public static ComponentModel Expand(
        ComponentSpec component,
        IReadOnlyDictionary<string, ExpressionValue> props,
        ExtensionContext context,
        ExtensionSet set,
        Func<string, ActionModel?> actions,
        IReadOnlyDictionary<string, IReadOnlyList<ExpandedNode>>? fill = null)
    {
        ArgumentNullException.ThrowIfNull(component);
        ArgumentNullException.ThrowIfNull(props);
        ArgumentNullException.ThrowIfNull(context);
        ArgumentNullException.ThrowIfNull(set);
        ArgumentNullException.ThrowIfNull(actions);
        Expander expander = new(context, set, actions, set.Sink);
        Dictionary<string, ExpressionValue> merged = new(StringComparer.Ordinal);
        foreach (ComponentProp declared in component.Props)
        {
            if (props.TryGetValue(declared.Name, out ExpressionValue? value))
            {
                merged[declared.Name] = value;
            }
            else if (declared.Required)
            {
                set.Sink.Report(component.Id, ExtensionCodes.Schema, $"prop {declared.Name} is required");
                return new ComponentModel(component, null);
            }
            else
            {
                merged[declared.Name] = ExpressionValue.Null;
            }
        }
        IReadOnlyList<ExpandedNode> nodes = expander.Nodes(component.Body, merged, 1, fill ?? EmptyFill, component.Id);
        if (expander.Aborted)
        {
            return new ComponentModel(component, null);
        }
        ExpandedNode? root = nodes.Count switch
        {
            0 => null,
            1 => nodes[0],
            _ => new ExpandedNode(ExpandedNodeKind.Column) { Children = nodes }
        };
        return new ComponentModel(component, root);
    }

    private static readonly IReadOnlyDictionary<string, IReadOnlyList<ExpandedNode>> EmptyFill =
        new Dictionary<string, IReadOnlyList<ExpandedNode>>(StringComparer.Ordinal);

    private sealed class Expander(
        ExtensionContext context,
        ExtensionSet set,
        Func<string, ActionModel?> actions,
        DiagnosticSink sink)
    {
        private int _nodes;
        private bool _aborted;
        private readonly List<string> _active = [];

        internal IReadOnlyList<ExpandedNode> Nodes(
            ComponentNode node,
            IReadOnlyDictionary<string, ExpressionValue> props,
            int depth,
            IReadOnlyDictionary<string, IReadOnlyList<ExpandedNode>> fill,
            string owner)
        {
            if (_aborted)
            {
                return [];
            }
            if (depth > ExtensionLimits.MaxComponentDepth)
            {
                _aborted = true;
                sink.Report(owner, ExtensionCodes.ComponentTooDeep,
                    $"{owner} nests deeper than {ExtensionLimits.MaxComponentDepth}");
                return [];
            }
            IExpressionContext values = new PropsContext(props, context.Now);
            switch (node)
            {
                case LayoutNode layout:
                    {
                        if (Count(owner) is null)
                        {
                            return [];
                        }
                        List<ExpandedNode> children = [];
                        foreach (ComponentNode child in layout.Children)
                        {
                            children.AddRange(Nodes(child, props, depth + 1, fill, owner));
                        }
                        return [new ExpandedNode(LayoutKind(layout.Kind))
                        {
                            Children = children,
                            Columns = layout.Columns ?? 0,
                            Title = layout.Title
                        }];
                    }
                case RepeatNode repeat:
                    {
                        if (Count(owner) is null)
                        {
                            return [];
                        }
                        int times = RepeatCount(props, repeat);
                        List<ExpandedNode> repeated = [];
                        for (int index = 0; index < times; index++)
                        {
                            repeated.AddRange(Nodes(repeat.Body, props, depth + 1, fill, owner));
                        }
                        return repeated;
                    }
                case WhenNode whenNode:
                    if (!whenNode.Test.EvaluateBool(values, sink, owner, fallback: false))
                    {
                        return [];
                    }
                    return Nodes(whenNode.Then, props, depth, fill, owner);
                case SlotNode slot:
                    return fill.TryGetValue(slot.Name, out IReadOnlyList<ExpandedNode>? filled) ? filled : [];
                case UseNode use:
                    return Use(use, values, depth, owner);
                case ControlNode control:
                    return Control(control, values, depth, owner);
                default:
                    return [];
            }
        }

        private IReadOnlyList<ExpandedNode> Use(UseNode use, IExpressionContext values, int depth, string owner)
        {
            if (Count(owner) is null)
            {
                return [];
            }
            ComponentSpec? used = set.Component(use.Component);
            if (used is null)
            {
                sink.Report(owner, ExtensionCodes.UnknownComponent, $"component {use.Component} is not in this document");
                return [new ExpandedNode(ExpandedNodeKind.Placeholder) { Placeholder = use.Component }];
            }
            if (_active.Contains(use.Component, StringComparer.Ordinal))
            {
                sink.Report(owner, ExtensionCodes.ComponentCycle, $"component cycle through {use.Component}");
                return [new ExpandedNode(ExpandedNodeKind.Placeholder) { Placeholder = use.Component }];
            }
            Dictionary<string, ExpressionValue> childProps = new(StringComparer.Ordinal);
            foreach (KeyValuePair<string, CompiledExpression> prop in use.Props)
            {
                childProps[prop.Key] = prop.Value.Evaluate(values, sink, owner);
            }
            Dictionary<string, IReadOnlyList<ExpandedNode>> fills = new(StringComparer.Ordinal);
            foreach (KeyValuePair<string, ComponentNode> slot in use.Fill)
            {
                fills[slot.Key] = Nodes(slot.Value, childProps, depth + 1, EmptyFill, owner);
            }
            _active.Add(use.Component);
            try
            {
                IReadOnlyList<ExpandedNode> nodes = Nodes(used.Body, childProps, depth + 1, fills, used.Id);
                return nodes;
            }
            finally
            {
                _active.RemoveAt(_active.Count - 1);
            }
        }

        private IReadOnlyList<ExpandedNode> Control(ControlNode control, IExpressionContext values, int depth, string owner)
        {
            _ = depth;
            if (Count(owner) is null)
            {
                return [];
            }
            if (!ExtensionLimits.ComponentControls.Contains(control.Control))
            {
                sink.Report(owner, ExtensionCodes.Schema, $"`{control.Control}` is not a catalog control");
                return [new ExpandedNode(ExpandedNodeKind.Placeholder) { Placeholder = control.Control, Text = control.Text }];
            }
            ExpressionValue value = control.Value?.Evaluate(values, sink, owner) ?? ExpressionValue.Null;
            ExpandedNode node = control.Control switch
            {
                "separator" => new ExpandedNode(ExpandedNodeKind.Separator),
                "heading" => new ExpandedNode(ExpandedNodeKind.Heading) { Text = control.Text ?? DisplayText.For(value) },
                "badge" => new ExpandedNode(ExpandedNodeKind.Badge) { Text = control.Text ?? DisplayText.For(value) },
                "progress" => new ExpandedNode(ExpandedNodeKind.Progress) { Percent = ExtensionFormatting.Progress(value) },
                "number" => new ExpandedNode(ExpandedNodeKind.Number) { Value = ExtensionFormatting.Value(value, "number", context.CurrencySymbol) },
                "currency" => new ExpandedNode(ExpandedNodeKind.Currency) { Value = ExtensionFormatting.Value(value, "currency", context.CurrencySymbol) },
                "date" => new ExpandedNode(ExpandedNodeKind.Date) { Value = ExtensionFormatting.Value(value, "date", context.CurrencySymbol) },
                "button" => Button(control, owner),
                _ => new ExpandedNode(ExpandedNodeKind.Label) { Text = control.Text ?? DisplayText.For(value) }
            };
            return [node];
        }

        private ExpandedNode Button(ControlNode control, string owner)
        {
            if (control.OnClick is null)
            {
                return new ExpandedNode(ExpandedNodeKind.Button) { Text = control.Text ?? string.Empty };
            }
            ActionModel? action = actions(control.OnClick);
            if (action is null)
            {
                sink.Report(owner, ExtensionCodes.UnknownAction, $"action {control.OnClick} is not in this document");
                return new ExpandedNode(ExpandedNodeKind.Placeholder) { Placeholder = control.OnClick, Text = control.Text };
            }
            return new ExpandedNode(ExpandedNodeKind.Button) { Text = control.Text ?? action.Label, Action = action };
        }

        private static int RepeatCount(IReadOnlyDictionary<string, ExpressionValue> props, RepeatNode repeat)
        {
            int count = props.TryGetValue(repeat.Over, out ExpressionValue? value)
                ? value switch
                {
                    IntValue integer when integer.Value > 0 => (int)Math.Min(integer.Value, int.MaxValue),
                    ListValue list => list.Values.Count,
                    _ => 0
                }
                : 0;
            return Math.Clamp(count, 0, repeat.Max);
        }

        /// <summary>Counts one node; null when the budget is spent (the expansion is refused whole).</summary>
        internal bool Aborted => _aborted;

        private int? Count(string owner)
        {
            _nodes++;
            if (_nodes > ExtensionLimits.MaxComponentNodes)
            {
                _aborted = true;
                sink.Report(owner, ExtensionCodes.ComponentTooLarge,
                    $"{owner} expands to more than {ExtensionLimits.MaxComponentNodes} nodes; the limit is {ExtensionLimits.MaxComponentNodes}");
                return null;
            }
            return _nodes;
        }
    }

    private static ExpandedNodeKind LayoutKind(string kind) => kind switch
    {
        "row" => ExpandedNodeKind.Row,
        "grid" => ExpandedNodeKind.Grid,
        "group" => ExpandedNodeKind.Group,
        _ => ExpandedNodeKind.Column
    };

    /// <summary>The props an expression inside a component reads as <c>doc</c>.</summary>
    private sealed class PropsContext(IReadOnlyDictionary<string, ExpressionValue> props, ExpressionDateTime now) : IExpressionContext
    {
        public ExpressionValue? Field(ExpressionRoot root, IReadOnlyList<string> names) =>
            root == ExpressionRoot.Doc && names.Count == 1 && props.TryGetValue(names[0], out ExpressionValue? value)
                ? value
                : null;

        public ExpressionDateTime Now { get; } = now;
    }
}
