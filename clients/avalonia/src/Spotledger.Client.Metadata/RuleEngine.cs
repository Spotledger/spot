using Spotledger.Client.Expressions;

namespace Spotledger.Client.Metadata;

/// <summary>
/// The fields whose rule state changed in one draft change, batched: the engine raises
/// this once per change, never once per field. A renderer re-reads these fields'
/// <see cref="FieldState.IsVisible"/>, <see cref="FieldState.IsMandatory"/> and
/// <see cref="FieldState.IsReadOnly"/> and repaints only them.
/// </summary>
public sealed class RulesChangedEventArgs : EventArgs
{
    /// <summary>Creates the batch.</summary>
    /// <param name="fields">The field paths whose rule state changed (a child row's
    /// field is <c>items[3].qty</c>, rows counted from 1).</param>
    public RulesChangedEventArgs(IReadOnlyList<string> fields) => Fields = fields;

    /// <summary>The field paths whose rule state changed, in evaluation order.</summary>
    public IReadOnlyList<string> Fields { get; }
}

/// <summary>
/// The instant field rules of one DocType, attached to a <see cref="DocumentDraft"/>
/// (ADR-28, ADR-068 §1 and A3): the field's <c>depends_on</c>,
/// <c>mandatory_depends_on</c> and <c>read_only_depends_on</c> run locally through the
/// shared expression language, so the client and the server agree without a round trip.
///
/// The expressions are parsed **once per metadata version** (a <see cref="DocTypeModel"/>
/// is one version); a parse error disables only that rule and becomes a
/// <see cref="RuleDiagnostic"/>, never an exception at the user. The engine builds the
/// field dependency map from the expressions' own field reads, so a change re-evaluates
/// only the rules that read the changed field, in a deterministic (dependency) order,
/// and the engine raises one batched <see cref="RulesChanged"/> per change.
///
/// The renderer only reads the resulting <see cref="FieldState"/>; nothing here knows
/// about controls, and nothing here decides what the server stores. Rule expressions
/// never change a value: only the three field states, plus the value a **new** document
/// or row starts with (<c>default_expr</c>, evaluated once, on creation).
/// </summary>
public sealed class RuleEngine
{
    /// <summary>The shared expression language version this client implements
    /// (<c>docs/expression-language.md</c>). A DocType whose <c>expr_version</c> is
    /// higher is treated as if it carried no expressions at all (ADR-068 A3.1).</summary>
    public const int SupportedExpressionVersion = 1;

    private readonly DocumentDraft _draft;
    private readonly TimeProvider _time;
    private readonly RuleSet _documentRules;
    private readonly Dictionary<string, RuleSet> _rowRules = new(StringComparer.Ordinal);
    private readonly Dictionary<DocumentDraft.TableDraft.RowDraft, Scope> _rowScopes = new();
    private readonly HashSet<string> _reported = new(StringComparer.Ordinal);
    private readonly List<RuleDiagnostic> _diagnostics = new();
    private long _evaluations;
    private long _fieldReads;

    internal RuleEngine(DocumentDraft draft, TimeProvider? time = null)
    {
        ArgumentNullException.ThrowIfNull(draft);
        _draft = draft;
        _time = time ?? TimeProvider.System;
        // Each model is enabled on its own: a DocType whose own fields carry no
        // expression may still have a child row DocType that does, and its row rules must
        // run (the document scope then evaluates an empty rule set). Gating the rows on
        // the document's expressions would silently ignore them.
        var documentEnabled = IsEnabled(draft.Model);
        var rowsEnabled = draft.Tables.Values.Any(table => table.RowModel is { } row && IsEnabled(row));
        ExpressionsEnabled = documentEnabled || rowsEnabled;
        _documentRules = RuleSet.For(draft.Model, documentEnabled);
        _diagnostics.AddRange(_documentRules.Diagnostics);
        if (!documentEnabled)
        {
            ReportUnsupportedVersion(draft.Model);
        }

        foreach (var table in draft.Tables.Values)
        {
            if (table.RowModel is null)
            {
                continue;
            }

            var enabled = IsEnabled(table.RowModel);
            var rules = RuleSet.For(table.RowModel, enabled);
            _rowRules[table.Field.Name] = rules;
            _diagnostics.AddRange(rules.Diagnostics);
            if (!enabled)
            {
                ReportUnsupportedVersion(table.RowModel);
            }
        }

        _documentScope = new Scope(_documentRules, name => _draft.Get(name), null, name => _draft.Fields.TryGetValue(name, out var state) ? state : null, string.Empty);
    }

    /// <summary>A model's expressions are evaluated only when the client implements the
    /// version they are written for and the model carries at least one.</summary>
    private static bool IsEnabled(DocTypeModel model) =>
        (model.ExprVersion is null or SupportedExpressionVersion)
        && model.Fields.Any(field => field.HasExpressions);

    /// <summary>Reports a model whose expressions exist but are written for a version
    /// this client does not implement; its rules are ignored.</summary>
    private void ReportUnsupportedVersion(DocTypeModel model)
    {
        if (model.Fields.Any(field => field.HasExpressions))
        {
            _diagnostics.Add(new RuleDiagnostic(
                model.Name,
                RuleKind.Visibility,
                RuleDiagnosticCode.UnsupportedExprVersion,
                $"The DocType's expression version {model.ExprVersion} is not the version this client implements ({SupportedExpressionVersion}); every rule expression is ignored."));
        }
    }

    private readonly Scope _documentScope;

    /// <summary>Raised once per draft change that alters at least one field's rule state,
    /// with every altered field path in the one batch.</summary>
    public event EventHandler<RulesChangedEventArgs>? RulesChanged;

    /// <summary>The expression language version the DocType's rules are written for
    /// (null when it carries none).</summary>
    public int? ExpressionVersion => _draft.Model.ExprVersion;

    /// <summary>True when this client evaluates at least one rule expression of the
    /// document or of a child row; false when neither carries any or their versions are
    /// newer than this client implements, in which case the static properties alone are
    /// in force. A model is enabled on its own, so a document without expressions does
    /// not stop a child row's rules from running.</summary>
    public bool ExpressionsEnabled { get; }

    /// <summary>Every problem found in the DocType's rule expressions, in field order: a
    /// parse error, a cycle, a result that is not a bool, an unsupported version. A
    /// diagnosed rule is skipped; the field keeps its static properties and the server's
    /// check on save stays authoritative.</summary>
    public IReadOnlyList<RuleDiagnostic> Diagnostics => _diagnostics;

    /// <summary>How many rule evaluations have run (the brief's "only the affected
    /// rules re-run" is observable through this counter).</summary>
    public long EvaluationCount => _evaluations;

    /// <summary>How many field reads the expressions have made.</summary>
    public long FieldReadCount => _fieldReads;

    /// <summary>
    /// An optional wrapper around the expression context, applied to every evaluation.
    /// It exists for diagnostics and tests (a counting context proves which rules read
    /// what); the engine itself never needs it.
    /// </summary>
    public Func<IExpressionContext, IExpressionContext>? Observe { get; set; }

    /// <summary>Re-evaluates every rule of the document and of every child row (after a
    /// load, a revert or a permission change) and sets the fields' rule state.</summary>
    internal void EvaluateAll()
    {
        if (!ExpressionsEnabled)
        {
            ApplyStaticStates(_draft.Model, _draft.Fields);
            return;
        }

        Evaluate(_documentScope, null);
        PruneRowScopes();
        foreach (var table in _draft.Tables.Values)
        {
            foreach (var row in table.Rows)
            {
                Evaluate(RowScope(table, row), null);
            }
        }
    }

    /// <summary>Re-evaluates the rules a change to <paramref name="field"/> affects, then
    /// raises one batched <see cref="RulesChanged"/> when any field's state moved.</summary>
    internal void RefreshAfter(string field)
    {
        if (!ExpressionsEnabled)
        {
            return;
        }

        var changed = new List<string>();
        Evaluate(_documentScope, _documentScope.Rules.RulesReading(ExpressionRoot.Doc, field));
        changed.AddRange(LastChanged(_documentScope));

        // A row rule may read the parent document: every row of every table is a
        // candidate, but only the rules that actually read this field re-run.
        foreach (var table in _draft.Tables.Values)
        {
            if (!_rowRules.TryGetValue(table.Field.Name, out var rules))
            {
                continue;
            }

            var seeds = rules.RulesReading(ExpressionRoot.Parent, field);
            if (seeds.Count == 0)
            {
                continue;
            }

            foreach (var row in table.Rows)
            {
                var scope = RowScope(table, row);
                Evaluate(scope, seeds);
                changed.AddRange(LastChanged(scope));
            }
        }

        Raise(changed);
    }

    /// <summary>Re-evaluates the rules a change to one row field affects, in that row.</summary>
    internal void RefreshAfterRow(DocumentDraft.TableDraft table, DocumentDraft.TableDraft.RowDraft row, string field)
    {
        if (!ExpressionsEnabled || !_rowRules.TryGetValue(table.Field.Name, out var rules))
        {
            return;
        }

        var scope = RowScope(table, row);
        Evaluate(scope, rules.RulesReading(ExpressionRoot.Doc, field));
        Raise(LastChanged(scope));
    }

    /// <summary>Applies the <c>default_expr</c> values of a new document, in dependency
    /// order, once (the static defaults were already applied by the draft's
    /// constructor).</summary>
    internal void ApplyCreationDefaults()
    {
        if (!ExpressionsEnabled)
        {
            return;
        }

        ApplyDefaults(_documentScope);
        foreach (var table in _draft.Tables.Values)
        {
            foreach (var row in table.Rows)
            {
                ApplyDefaults(RowScope(table, row));
            }
        }
    }

    /// <summary>Applies a new child row's <c>default_expr</c> values, in dependency order,
    /// and evaluates its rules for the first time.</summary>
    internal void OnRowAdded(DocumentDraft.TableDraft table, DocumentDraft.TableDraft.RowDraft row)
    {
        if (!ExpressionsEnabled)
        {
            return;
        }

        var scope = RowScope(table, row);
        ApplyDefaults(scope);
        Evaluate(scope, null);
    }

    /// <summary>Forgets the rule state of rows the draft no longer holds (a load or a
    /// revert replaces every row object).</summary>
    internal void PruneRowScopes()
    {
        if (_rowScopes.Count == 0)
        {
            return;
        }

        var live = new HashSet<DocumentDraft.TableDraft.RowDraft>(ReferenceEqualityComparer.Instance);
        foreach (var table in _draft.Tables.Values)
        {
            foreach (var row in table.Rows)
            {
                live.Add(row);
            }
        }

        foreach (var row in _rowScopes.Keys.Where(candidate => !live.Contains(candidate)).ToArray())
        {
            _rowScopes.Remove(row);
        }
    }

    private void ApplyDefaults(Scope scope)
    {
        foreach (var rule in scope.Rules.Defaults)
        {
            if (rule.Node is null)
            {
                continue;
            }

            var value = EvaluateValue(rule, scope);
            if (value is null)
            {
                continue;
            }

            if (!ExpressionValues.TryToFieldValue(value, rule.Field, out var converted))
            {
                Report(RuleDiagnosticCode.DefaultType, rule,
                    $"The default expression produced {value.TypeName}, which the field {rule.Field.Label} cannot hold; the static default stands.");
                continue;
            }

            var state = scope.State(rule.Field.Name);
            state?.ReplaceLoadedValue(converted);
        }
    }

    private Scope RowScope(DocumentDraft.TableDraft table, DocumentDraft.TableDraft.RowDraft row)
    {
        if (!_rowScopes.TryGetValue(row, out var scope))
        {
            var rules = _rowRules[table.Field.Name];
            scope = new Scope(
                rules,
                name => row.GetValue(name),
                name => _draft.Get(name),
                name => row.Values.TryGetValue(name, out var state) ? state : null,
                string.Empty);
            _rowScopes[row] = scope;
        }

        // Rows move and are removed: the path a diagnostic or a change batch reports is
        // the row's position now, not the one it had when the scope was created.
        RefreshPath(scope, table.Field.Name, Ordinal(table, row));
        return scope;
    }

    private static int Ordinal(DocumentDraft.TableDraft table, DocumentDraft.TableDraft.RowDraft row)
    {
        for (var i = 0; i < table.Rows.Count; i++)
        {
            if (ReferenceEquals(table.Rows[i], row))
            {
                return i + 1;
            }
        }

        return table.Rows.Count;
    }

    /// <summary>
    /// One evaluation pass over a scope, in the dependency order the rule set computed.
    /// <paramref name="seed"/> is null for a full pass; otherwise only the rules whose
    /// field reads changed run, plus the rules a changed rule state makes relevant —
    /// "only the affected rules re-run" (ADR-28).
    /// </summary>
    private void Evaluate(Scope scope, IReadOnlyCollection<int>? seed)
    {
        var pending = seed is null ? null : new HashSet<int>(seed);
        scope.Changed.Clear();
        foreach (var fieldName in scope.Rules.Order)
        {
            var memory = scope.MemoryFor(fieldName);
            var ranAny = false;
            foreach (var rule in scope.Rules.RulesOf(fieldName))
            {
                if (pending is not null && !pending.Contains(rule.Index))
                {
                    continue;
                }

                ranAny = true;
                var truth = EvaluateTruth(rule, scope);
                switch (rule.Kind)
                {
                    case RuleKind.Visibility:
                        memory.Visible = truth;
                        break;
                    case RuleKind.Mandatory:
                        memory.Mandatory = truth;
                        break;
                    case RuleKind.ReadOnly:
                        memory.ReadOnly = truth;
                        break;
                    default:
                        break;
                }
            }

            if (!ranAny)
            {
                continue;
            }

            var field = scope.Rules.Field(fieldName);
            var state = Combine(field, memory);
            if (memory.HasState && memory.State == state)
            {
                continue;
            }

            memory.State = state;
            memory.HasState = true;
            if (scope.State(fieldName) is { } fieldState)
            {
                fieldState.IsVisible = state.Visible;
                fieldState.IsMandatory = state.Mandatory;
                fieldState.IsReadOnly = state.ReadOnly;
            }

            scope.Changed.Add(fieldName);
            if (pending is not null)
            {
                // A rule state that moved makes the rules reading that field relevant
                // again (a chain of rules resolves in one change).
                foreach (var dependent in scope.Rules.RulesReading(ExpressionRoot.Doc, fieldName))
                {
                    pending.Add(dependent);
                }
            }
        }
    }

    private static FieldStateFields Combine(FieldModel field, FieldMemory memory)
    {
        var visible = !field.Hints.Hidden && memory.Visible != false;
        var mandatory = field.Required || memory.Mandatory == true;
        var readOnly = field.ReadOnly || memory.ReadOnly == true;
        return new FieldStateFields(visible, mandatory, readOnly);
    }

    private bool? EvaluateTruth(ParsedRule rule, Scope scope)
    {
        var value = EvaluateValue(rule, scope);
        return value switch
        {
            null => null,
            BoolValue boolean => boolean.Value,
            NullValue => false,
            _ => NotABool(rule, value),
        };
    }

    private bool? NotABool(ParsedRule rule, ExpressionValue value)
    {
        Report(RuleDiagnosticCode.NotABool, rule,
            $"{rule.Kind} must evaluate to a bool, not {value.TypeName}; the rule is ignored.");
        return null;
    }

    private ExpressionValue? EvaluateValue(ParsedRule rule, Scope scope)
    {
        if (rule.Node is null)
        {
            return null;
        }

        _evaluations++;
        IExpressionContext context = new RuleContext(scope, _time, this);
        if (Observe is not null)
        {
            context = Observe(context);
        }

        try
        {
            return Expression.Evaluate(rule.Node, context);
        }
        catch (ExpressionEvaluationException error)
        {
            Report(RuleDiagnosticCode.Evaluation, rule, error.MessageText);
            return null;
        }
    }

    private void Report(string code, ParsedRule rule, string message)
    {
        var key = $"{code}|{rule.Field.Name}|{rule.Kind}";
        if (_reported.Add(key))
        {
            _diagnostics.Add(new RuleDiagnostic(rule.Field.Name, rule.Kind, code, message));
        }
    }

    private void Raise(IEnumerable<string> changed)
    {
        var fields = changed.ToArray();
        if (fields.Length > 0)
        {
            RulesChanged?.Invoke(this, new RulesChangedEventArgs(Array.AsReadOnly(fields)));
        }
    }

    /// <summary>Maps a row scope's changed field names to their tree paths, refreshing
    /// the prefix from the row's current position (rows move and are removed).</summary>
    private static void RefreshPath(Scope scope, string tableName, int ordinal) =>
        scope.PathPrefix = $"{tableName}[{ordinal}].";

    private static IReadOnlyList<string> LastChanged(Scope scope)
    {
        if (scope.Changed.Count == 0)
        {
            return Array.Empty<string>();
        }

        var paths = new List<string>(scope.Changed.Count);
        foreach (var field in scope.Changed)
        {
            paths.Add(scope.PathOf(field));
        }

        return paths;
    }

    private static void ApplyStaticStates(DocTypeModel model, IReadOnlyDictionary<string, FieldState> states)
    {
        foreach (var field in model.Fields)
        {
            if (!states.TryGetValue(field.Name, out var state))
            {
                continue;
            }

            state.IsVisible = !field.Hints.Hidden;
            state.IsMandatory = field.Required;
            state.IsReadOnly = field.ReadOnly;
        }
    }

    /// <summary>One scope's rules plus the way it reads values and writes field states:
    /// the document itself, or one child row (whose <c>parent</c> is the document).</summary>
    private sealed class Scope
    {
        public Scope(
            RuleSet rules,
            Func<string, FieldValue?> value,
            Func<string, FieldValue?>? parent,
            Func<string, FieldState?> state,
            string pathPrefix)
        {
            Rules = rules;
            Value = value;
            Parent = parent;
            State = state;
            PathPrefix = pathPrefix;
        }

        public RuleSet Rules { get; }

        public Func<string, FieldValue?> Value { get; }

        public Func<string, FieldValue?>? Parent { get; }

        public Func<string, FieldState?> State { get; }

        public string PathPrefix { get; set; }

        public Dictionary<string, FieldMemory> Memory { get; } = new(StringComparer.Ordinal);

        public List<string> Changed { get; } = new();

        public FieldMemory MemoryFor(string field)
        {
            if (!Memory.TryGetValue(field, out var memory))
            {
                memory = new FieldMemory();
                Memory[field] = memory;
            }

            return memory;
        }

        public string PathOf(string field) => PathPrefix.Length == 0 ? field : PathPrefix + field;
    }

    /// <summary>The last result of each of a field's rules, and the state they combined to.</summary>
    private sealed class FieldMemory
    {
        public bool? Visible { get; set; }

        public bool? Mandatory { get; set; }

        public bool? ReadOnly { get; set; }

        public bool HasState { get; set; }

        public FieldStateFields State { get; set; }
    }

    private readonly record struct FieldStateFields(bool Visible, bool Mandatory, bool ReadOnly);

    /// <summary>The expression context over one scope: <c>doc</c> is the scope's own
    /// values, <c>parent</c> the document above a child row, and <c>now</c> the injected
    /// clock's instant.</summary>
    private sealed class RuleContext : IExpressionContext
    {
        private readonly Scope _scope;
        private readonly RuleEngine _engine;

        public RuleContext(Scope scope, TimeProvider time, RuleEngine engine)
        {
            _scope = scope;
            _engine = engine;
            if (time.GetUtcNow() is var now
                && ExpressionDateTime.FromMicros((now.UtcTicks - DateTimeOffset.UnixEpoch.UtcTicks) / 10) is { } moment)
            {
                Now = moment;
            }
        }

        public ExpressionDateTime Now { get; }

        public ExpressionValue? Field(ExpressionRoot root, IReadOnlyList<string> names)
        {
            if (names.Count == 0)
            {
                return null;
            }

            if (root == ExpressionRoot.Parent && _scope.Parent is null)
            {
                // A parent path on the document itself has no value (never an error).
                return null;
            }

            _engine._fieldReads++;
            var read = root == ExpressionRoot.Doc ? _scope.Value(names[0]) : _scope.Parent!(names[0]);

            // A longer path reads through a link: this host knows no link traversal, so
            // the path has no value (the language's null), never an error.
            return names.Count > 1 ? null : ExpressionValues.ToExpression(read);
        }
    }
}
