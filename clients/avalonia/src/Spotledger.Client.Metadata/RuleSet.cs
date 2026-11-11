using System.Runtime.CompilerServices;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Metadata;

/// <summary>One field read of an expression: which root and which field name.</summary>
internal readonly record struct FieldReference(ExpressionRoot Root, string Name);

/// <summary>One rule expression of one field, parsed once per metadata version.</summary>
internal sealed class ParsedRule
{
    /// <summary>The field the rule belongs to.</summary>
    public required FieldModel Field { get; init; }

    /// <summary>Which rule this is.</summary>
    public required RuleKind Kind { get; init; }

    /// <summary>The source text, kept for diagnostics.</summary>
    public required string Source { get; init; }

    /// <summary>The rule's position in <see cref="RuleSet.Rules"/>.</summary>
    public required int Index { get; init; }

    /// <summary>The parsed tree; null when the expression did not parse (the rule is
    /// then disabled and its diagnostic is in <see cref="RuleSet.Diagnostics"/>).</summary>
    public ExpressionNode? Node { get; init; }
}

/// <summary>
/// Every rule expression of one metadata version, parsed once and ordered: the field
/// dependency map (which rule reads which field), the deterministic evaluation order and
/// the problems found while building it. Immutable and cached per
/// <see cref="DocTypeModel"/> instance — a new metadata version is a new model, so this
/// is the brief's "parsed once per metadata version".
/// </summary>
internal sealed class RuleSet
{
    private static readonly ConditionalWeakTable<DocTypeModel, RuleSet> Cache = new();

    private readonly Dictionary<string, IReadOnlyList<ParsedRule>> _byField = new(StringComparer.Ordinal);
    private readonly Dictionary<(ExpressionRoot Root, string Name), List<int>> _reverse = new();
    private readonly Dictionary<string, FieldModel> _fields = new(StringComparer.Ordinal);

    private RuleSet(DocTypeModel model)
    {
        Model = model;
        foreach (var field in model.Fields)
        {
            _fields[field.Name] = field;
        }
    }

    /// <summary>The DocType the rules belong to.</summary>
    public DocTypeModel Model { get; }

    /// <summary>The boolean rules (visibility, mandatory, read-only) in declaration order.</summary>
    public List<ParsedRule> Rules { get; } = new();

    /// <summary>The fields that carry rules, in dependency order (each field's rules run
    /// after the rules of the fields they read).</summary>
    public List<string> Order { get; } = new();

    /// <summary>The <c>default_expr</c> rules, in the order they must be evaluated.</summary>
    public List<ParsedRule> Defaults { get; } = new();

    /// <summary>What could not be built, in field order.</summary>
    public List<RuleDiagnostic> Diagnostics { get; } = new();

    /// <summary>The parsed rule set of a metadata version (cached per model instance).</summary>
    public static RuleSet For(DocTypeModel model, bool enabled)
    {
        ArgumentNullException.ThrowIfNull(model);
        return enabled ? Cache.GetValue(model, Build) : new RuleSet(model);
    }

    /// <summary>The rules of one field, in kind order (visibility, mandatory, read-only).</summary>
    public IReadOnlyList<ParsedRule> RulesOf(string field) =>
        _byField.TryGetValue(field, out var rules) ? rules : Array.Empty<ParsedRule>();

    /// <summary>The field's model.</summary>
    public FieldModel Field(string field) => _fields[field];

    /// <summary>The rules that read <paramref name="name"/> at the given root: the ones a
    /// change to that field must re-run.</summary>
    public IReadOnlyList<int> RulesReading(ExpressionRoot root, string name) =>
        _reverse.TryGetValue((root, name), out var rules) ? rules : Array.Empty<int>();

    private static RuleSet Build(DocTypeModel model)
    {
        var set = new RuleSet(model);
        var index = 0;

        ParsedRule? Parse(FieldModel field, RuleKind kind, string? source)
        {
            if (source is null)
            {
                return null;
            }

            var position = index++;
            try
            {
                return new ParsedRule
                {
                    Field = field,
                    Kind = kind,
                    Source = source,
                    Index = position,
                    Node = ExpressionParser.Parse(source),
                };
            }
            catch (ExpressionParseException error)
            {
                // A bad expression disables only that rule: the field keeps its static
                // properties and the form keeps working (ADR-068 §1.4).
                set.Diagnostics.Add(new RuleDiagnostic(
                    field.Name,
                    kind,
                    RuleDiagnosticCode.Parse,
                    $"{error.Code}: {error.MessageText}",
                    error.Offset));
                return null;
            }
        }

        foreach (var field in model.Fields)
        {
            var rules = new List<ParsedRule>(3);
            if (Parse(field, RuleKind.Visibility, field.DependsOn) is { } visibility)
            {
                rules.Add(visibility);
            }

            if (Parse(field, RuleKind.Mandatory, field.MandatoryDependsOn) is { } mandatory)
            {
                rules.Add(mandatory);
            }

            if (Parse(field, RuleKind.ReadOnly, field.ReadOnlyDependsOn) is { } readOnly)
            {
                rules.Add(readOnly);
            }

            if (rules.Count > 0)
            {
                set._byField[field.Name] = rules.AsReadOnly();
                set.Rules.AddRange(rules);
            }

            if (Parse(field, RuleKind.Default, field.DefaultExpr) is { } defaultRule)
            {
                set.Defaults.Add(defaultRule);
            }
        }

        set.BuildReverseMap();
        set.Order.AddRange(set.OrderFields(set._byField.Keys, rulesOnly: true));
        set.OrderDefaults();
        return set;
    }

    private void BuildReverseMap()
    {
        foreach (var rule in Rules)
        {
            if (rule.Node is null)
            {
                continue;
            }

            foreach (var read in Reads(rule))
            {
                if (!_reverse.TryGetValue((read.Root, read.Name), out var readers))
                {
                    readers = new List<int>();
                    _reverse[(read.Root, read.Name)] = readers;
                }

                if (!readers.Contains(rule.Index))
                {
                    readers.Add(rule.Index);
                }
            }
        }
    }

    /// <summary>The fields of the given rules, ordered so that a field's rules run after
    /// the rules of every field they read (a chain A → B → C resolves in one pass). A
    /// cycle is reported — the result would depend on the order — and the remaining
    /// fields keep their declaration order so evaluation stays total and deterministic.
    /// </summary>
    private List<string> OrderFields(IEnumerable<string> candidates, bool rulesOnly)
    {
        var declarationOrder = new Dictionary<string, int>(StringComparer.Ordinal);
        for (var i = 0; i < Model.Fields.Count; i++)
        {
            declarationOrder[Model.Fields[i].Name] = i;
        }

        var nodes = new HashSet<string>(StringComparer.Ordinal);
        foreach (var candidate in candidates)
        {
            if (declarationOrder.ContainsKey(candidate))
            {
                nodes.Add(candidate);
            }
        }

        var dependsOn = new Dictionary<string, HashSet<string>>(StringComparer.Ordinal);
        var dependents = new Dictionary<string, List<string>>(StringComparer.Ordinal);
        foreach (var node in nodes)
        {
            dependsOn[node] = new HashSet<string>(StringComparer.Ordinal);
            dependents[node] = new List<string>();
        }

        var rules = rulesOnly ? Rules : Defaults;
        foreach (var rule in rules)
        {
            if (rule.Node is null || !nodes.Contains(rule.Field.Name))
            {
                continue;
            }

            foreach (var read in Reads(rule))
            {
                if (read.Root != ExpressionRoot.Doc
                    || !nodes.Contains(read.Name)
                    || string.Equals(read.Name, rule.Field.Name, StringComparison.Ordinal))
                {
                    // A rule never depends on itself; a parent read belongs to the row
                    // scope; a field without rules needs no ordering.
                    continue;
                }

                if (dependsOn[rule.Field.Name].Add(read.Name))
                {
                    dependents[read.Name].Add(rule.Field.Name);
                }
            }
        }

        var pending = new Dictionary<string, int>(StringComparer.Ordinal);
        var ready = new SortedSet<int>();
        var byIndex = new Dictionary<int, string>();
        foreach (var node in nodes)
        {
            pending[node] = dependsOn[node].Count;
        }

        foreach (var node in nodes.Where(node => dependsOn[node].Count == 0))
        {
            ready.Add(declarationOrder[node]);
            byIndex[declarationOrder[node]] = node;
        }

        var order = new List<string>(nodes.Count);
        var placed = new HashSet<string>(StringComparer.Ordinal);
        while (ready.Count > 0)
        {
            var next = ready.Min;
            ready.Remove(next);
            var field = byIndex[next];
            order.Add(field);
            placed.Add(field);
            foreach (var dependent in dependents[field])
            {
                if (--pending[dependent] == 0)
                {
                    ready.Add(declarationOrder[dependent]);
                    byIndex[declarationOrder[dependent]] = dependent;
                }
            }
        }

        if (placed.Count == nodes.Count)
        {
            return order;
        }

        var stuck = nodes
            .Where(node => !placed.Contains(node))
            .OrderBy(node => declarationOrder[node])
            .ToList();
        if (rulesOnly)
        {
            var kind = Rules.FirstOrDefault(rule => string.Equals(rule.Field.Name, stuck[0], StringComparison.Ordinal))?.Kind
                ?? RuleKind.Visibility;
            Diagnostics.Add(new RuleDiagnostic(
                stuck[0],
                kind,
                RuleDiagnosticCode.Cycle,
                $"The rules of {string.Join(", ", stuck)} depend on each other in a cycle; their result depends on the order of evaluation."));
        }

        order.AddRange(stuck);
        return order;
    }

    private void OrderDefaults()
    {
        var order = OrderFields(Defaults.Select(rule => rule.Field.Name), rulesOnly: false);
        var byField = new Dictionary<string, ParsedRule>(StringComparer.Ordinal);
        foreach (var rule in Defaults)
        {
            byField.TryAdd(rule.Field.Name, rule);
        }

        Defaults.Clear();
        foreach (var field in order)
        {
            if (byField.Remove(field, out var rule))
            {
                Defaults.Add(rule);
            }
        }
    }

    private static List<FieldReference> Reads(ParsedRule rule)
    {
        var reads = new List<FieldReference>();
        if (rule.Node is not null)
        {
            CollectReads(rule.Node, reads);
        }

        return reads;
    }

    /// <summary>Every field path an expression reads (its first name; a longer path
    /// reads through a link, which this host does not traverse).</summary>
    private static void CollectReads(ExpressionNode node, List<FieldReference> into)
    {
        switch (node)
        {
            case PathNode path:
                if (path.Names.Count > 0)
                {
                    var reference = new FieldReference(path.Root, path.Names[0]);
                    if (!into.Contains(reference))
                    {
                        into.Add(reference);
                    }
                }

                break;
            case UnaryNode unary:
                CollectReads(unary.Inner, into);
                break;
            case BinaryNode binary:
                CollectReads(binary.Left, into);
                CollectReads(binary.Right, into);
                break;
            case TernaryNode ternary:
                CollectReads(ternary.Condition, into);
                CollectReads(ternary.WhenTrue, into);
                CollectReads(ternary.WhenFalse, into);
                break;
            case CallNode call:
                foreach (var argument in call.Arguments)
                {
                    CollectReads(argument, into);
                }

                break;
            case ListNode list:
                foreach (var item in list.Items)
                {
                    CollectReads(item, into);
                }

                break;
            default:
                break;
        }
    }
}
