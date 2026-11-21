namespace Spotledger.Client.Designer;

/// <summary>
/// One edit the designer asks the server's draft to make.
/// </summary>
/// <remarks>
/// The shapes here mirror `spot-nocode-model`'s `DraftOp` (WP-DX-06.T1) one for one, and
/// <see cref="Op"/> carries that crate's own stable op name, so a test can compare the
/// commands the designer emitted with the server-side golden sequence without translating
/// anything. The designer never applies an op locally: it sends it, and the server either
/// returns the changed draft or a refusal.
/// </remarks>
public abstract record DesignerOp
{
    /// <summary>The server's stable op name (the value `DraftOp::name()` returns).</summary>
    public abstract string Op { get; }

    /// <summary>The child DocType the op targets, or null for the root DocType.</summary>
    public virtual string? Child => null;
}

/// <summary>Adds a field to the root or a child DocType.</summary>
public sealed record AddFieldOp(string Name, string Label, string Type, string? InsertAfter = null, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "add_field";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Renames a field's storage name.</summary>
public sealed record RenameFieldOp(string Name, string To, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "rename_field";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Removes a field; carries the server's data-loss confirmation token.</summary>
public sealed record RemoveFieldOp(string Name, string Confirmation, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "remove_field";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Changes a field's type, with the sample statistics the server classifies on.</summary>
public sealed record ChangeTypeOp(string Name, string Type, SampleStats Stats, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "change_type";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Sets a select field's values, in order.</summary>
public sealed record SetOptionsOp(string Name, IReadOnlyList<string> Options, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "set_options";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Marks a field required (or not).</summary>
public sealed record SetRequiredOp(string Name, bool Required, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "set_required";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Sets a field's default; null clears it.</summary>
public sealed record SetDefaultOp(string Name, string? Default, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "set_default";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Reorders a field in the definition's field list.</summary>
public sealed record ReorderFieldOp(string Name, string? After = null, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "reorder_field";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Adds a child DocType and the table field that points at it.</summary>
public sealed record AddChildTableOp(string Name, string Label, string ChildDoctypeName, string ChildTitle, string? InsertAfter = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "add_child_table";
}

/// <summary>Adds a link field pointing at an existing DocType.</summary>
public sealed record AddLinkOp(string Name, string Label, string Target, string? InsertAfter = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "add_link";
}

/// <summary>Sets the DocType's naming series pattern; null clears it.</summary>
public sealed record SetNamingSeriesOp(string? Naming) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "set_naming_series";
}

/// <summary>Chooses the permissions template (who may read, write and submit).</summary>
public sealed record SetPermissionsTemplateOp(string Template) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "set_permissions_template";
}

/// <summary>Adds a form section, at the top level or into a tab.</summary>
public sealed record AddSectionOp(string Id, string Title, int? Columns = null, string? TabId = null, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "add_form_section";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Adds a tab at the end of the form.</summary>
public sealed record AddTabOp(string Id, string Title, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "add_form_tab";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Sets a section's column count (its layout span).</summary>
public sealed record SetSectionColumnsOp(string SectionId, int Columns, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "set_section_columns";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Moves a field into a section, optionally anchored after another field.</summary>
public sealed record MoveInLayoutOp(string Field, string SectionId, string? After = null, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "move_field_in_layout";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>Sets a field's depends-on rule from the visual builder's rows; null clears it.</summary>
public sealed record SetDependsOnOp(string Name, ConditionExpression? Condition, string? ChildDoctype = null) : DesignerOp
{
    /// <inheritdoc />
    public override string Op => "set_depends_on";

    /// <inheritdoc />
    public override string? Child => ChildDoctype;
}

/// <summary>
/// How the rows of a visual condition combine: every row (and) or at least one (or).
/// </summary>
public enum ConditionJoin
{
    /// <summary>Every row must hold.</summary>
    And,

    /// <summary>At least one row must hold.</summary>
    Or,
}

/// <summary>What a condition row tests; the server's operator names are in <see cref="ConditionSyntax"/>.</summary>
public enum ConditionOperator
{
    /// <summary>The field holds a value.</summary>
    IsSet,

    /// <summary>The field holds no value.</summary>
    IsNotSet,

    /// <summary>Equals.</summary>
    Equals,

    /// <summary>Does not equal.</summary>
    NotEquals,

    /// <summary>Greater than.</summary>
    GreaterThan,

    /// <summary>At least.</summary>
    GreaterOrEqual,

    /// <summary>Less than.</summary>
    LessThan,

    /// <summary>At most.</summary>
    LessOrEqual,

    /// <summary>Is one of the listed values.</summary>
    In,
}

/// <summary>
/// A literal in a condition row. <see cref="Kind"/> is the server's value tag
/// (<c>text</c>, <c>int</c> or <c>bool</c>); the designer only formats it for display.
/// </summary>
public sealed record ConditionValue(string Kind, string Text)
{
    /// <summary>A text literal.</summary>
    public static ConditionValue OfText(string text) => new("text", text);

    /// <summary>A whole-number literal.</summary>
    public static ConditionValue OfInteger(long value) =>
        new("int", value.ToString(System.Globalization.CultureInfo.InvariantCulture));

    /// <summary>A true/false literal.</summary>
    public static ConditionValue OfBool(bool value) => new("bool", value ? "true" : "false");

    /// <summary>The value as a person reads it, without the expression language's quoting.</summary>
    public override string ToString() => $"\"{Text}\"";
}

/// <summary>One row of the visual condition builder: a field, an operator and its value(s).</summary>
public sealed record ConditionRow(string Field, ConditionOperator Operator, IReadOnlyList<ConditionValue> Values);

/// <summary>The visual condition as data. The server compiles it to expression text.</summary>
public sealed record ConditionExpression(ConditionJoin Join, IReadOnlyList<ConditionRow> Rows)
{
    /// <summary>A one-row condition.</summary>
    public static ConditionExpression FromRow(ConditionRow row) => new(ConditionJoin.And, new[] { row });
}

/// <summary>Wire names for the condition vocabulary, exactly as `spot-nocode-model` spells them.</summary>
public static class ConditionSyntax
{
    /// <summary>The server's operator name.</summary>
    public static string Name(ConditionOperator op) => op switch
    {
        ConditionOperator.IsSet => "is_set",
        ConditionOperator.IsNotSet => "is_not_set",
        ConditionOperator.Equals => "equals",
        ConditionOperator.NotEquals => "not_equals",
        ConditionOperator.GreaterThan => "greater_than",
        ConditionOperator.GreaterOrEqual => "greater_or_equal",
        ConditionOperator.LessThan => "less_than",
        ConditionOperator.LessOrEqual => "less_or_equal",
        ConditionOperator.In => "in",
        _ => "equals",
    };

    /// <summary>The server's join name.</summary>
    public static string Name(ConditionJoin join) => join == ConditionJoin.Or ? "or" : "and";

    /// <summary>How many values the operator takes; <c>null</c> for "any number" (<see cref="ConditionOperator.In"/>).</summary>
    public static int? ValueCount(ConditionOperator op) => op switch
    {
        ConditionOperator.IsSet or ConditionOperator.IsNotSet => 0,
        ConditionOperator.In => null,
        _ => 1,
    };

    /// <summary>A person-readable label for the operator.</summary>
    public static string Label(ConditionOperator op) => op switch
    {
        ConditionOperator.IsSet => "is set",
        ConditionOperator.IsNotSet => "is empty",
        ConditionOperator.Equals => "is",
        ConditionOperator.NotEquals => "is not",
        ConditionOperator.GreaterThan => "is more than",
        ConditionOperator.GreaterOrEqual => "is at least",
        ConditionOperator.LessThan => "is less than",
        ConditionOperator.LessOrEqual => "is at most",
        ConditionOperator.In => "is one of",
        _ => "is",
    };
}

/// <summary>
/// What the existing values of a field look like, measured by the server. The designer sends
/// it with a type change; it never counts rows itself. Money and quantities are whole counts,
/// never averages, so no floating point is involved (ADR-12).
/// </summary>
public sealed record SampleStats(long Rows = 0, long NonNull = 0, long? MaxTextLength = null, bool Fractional = false, long Unparsable = 0)
{
    /// <summary>No values measured.</summary>
    public static SampleStats Empty { get; } = new();
}
