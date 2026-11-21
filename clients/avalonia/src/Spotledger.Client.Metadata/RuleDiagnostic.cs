namespace Spotledger.Client.Metadata;

/// <summary>Which metadata rule a <see cref="RuleDiagnostic"/> is about.</summary>
public enum RuleKind
{
    /// <summary><c>depends_on</c>: whether the field is shown.</summary>
    Visibility,

    /// <summary><c>mandatory_depends_on</c>: whether the field is required.</summary>
    Mandatory,

    /// <summary><c>read_only_depends_on</c>: whether the field is read-only.</summary>
    ReadOnly,

    /// <summary><c>default_expr</c>: the value a new document or row starts with.</summary>
    Default,
}

/// <summary>
/// A problem with one field's rule expression, reported instead of thrown (ADR-068 §1:
/// a bad expression must never break the form). A parse error disables only that rule;
/// the field keeps its static properties, and the server's own check on save stays
/// authoritative. <see cref="RuleEngine.Diagnostics"/> holds every one found for the
/// metadata version, in field order.
/// </summary>
/// <param name="FieldPath">The field the rule belongs to (<c>discount_reason</c>, and
/// <c>items[2].qty</c> for a child row).</param>
/// <param name="Kind">Which rule the problem belongs to.</param>
/// <param name="Code">The stable code (<see cref="RuleDiagnosticCode"/>).</param>
/// <param name="Message">What is wrong, for the log and the form designer.</param>
/// <param name="Offset">The character offset of the offending token, when the language
/// reported one; null otherwise.</param>
public sealed record RuleDiagnostic(string FieldPath, RuleKind Kind, string Code, string Message, int? Offset = null)
{
    /// <inheritdoc/>
    public override string ToString() => Offset is { } offset
        ? $"{FieldPath} ({Kind}): {Code} at {offset}: {Message}"
        : $"{FieldPath} ({Kind}): {Code}: {Message}";
}

/// <summary>The stable diagnostic codes the rule engine reports.</summary>
public static class RuleDiagnosticCode
{
    /// <summary>The expression does not parse (the language's own code is in the message).</summary>
    public const string Parse = "rules.parse";

    /// <summary>The expression parsed but did not evaluate (for example a type error
    /// the values reached at run time).</summary>
    public const string Evaluation = "rules.evaluation";

    /// <summary>The rule's expression did not evaluate to a bool (and was not null).</summary>
    public const string NotABool = "rules.not_a_bool";

    /// <summary>Rules depend on each other in a cycle: the result would depend on the
    /// order of evaluation, so the cycle is reported.</summary>
    public const string Cycle = "rules.cycle";

    /// <summary>The DocType's <c>expr_version</c> is not the language version this
    /// client implements, so every rule expression of the DocType is ignored.</summary>
    public const string UnsupportedExprVersion = "rules.unsupported_expr_version";

    /// <summary>A <c>default_expr</c> produced a value of a type the field cannot hold.</summary>
    public const string DefaultType = "rules.default_type";
}
