namespace Spotledger.Designer.Reports;

/// <summary>How a field is shown in the palette, in terms a business user knows.</summary>
public enum OfferKind
{
    /// <summary>Words.</summary>
    Text,

    /// <summary>A whole number.</summary>
    Whole,

    /// <summary>An exact decimal (money, quantities).</summary>
    Exact,

    /// <summary>A calendar date.</summary>
    Day,

    /// <summary>Yes or no.</summary>
    Bool,

    /// <summary>A link to another record.</summary>
    Link,

    /// <summary>Anything else.</summary>
    Other,
}

/// <summary>One field the author may report on.</summary>
public sealed record FieldOffer(string Path, string Label, OfferKind Kind, bool Groupable, bool Summarisable);

/// <summary>A link the author may follow, with the fields reachable through it.</summary>
public sealed record JoinOffer(string Alias, string Label, string DocType, string On, IReadOnlyList<FieldOffer> Fields);

/// <summary>Everything the server lets this author put in a report over a base DocType.</summary>
public sealed record FieldOffers(string DocType, string Label, IReadOnlyList<FieldOffer> Fields, IReadOnlyList<JoinOffer> Joins);

/// <summary>A pickable row of the field palette: a field with its person-readable link path.</summary>
public sealed record FieldChoice(string Path, string DisplayName, FieldOffer Offer, string? JoinAlias)
{
    /// <summary>The row as the picker shows it.</summary>
    public override string ToString() => DisplayName;
}

/// <summary>An aggregate a column carries.</summary>
public enum Aggregate
{
    /// <summary>Sum of the values.</summary>
    Sum,

    /// <summary>Number of rows.</summary>
    Count,

    /// <summary>Smallest value.</summary>
    Min,

    /// <summary>Largest value.</summary>
    Max,

    /// <summary>Exact average.</summary>
    Avg,
}

/// <summary>The comparison a filter carries.</summary>
public enum Compare
{
    /// <summary>Equal.</summary>
    Eq,

    /// <summary>Not equal.</summary>
    Ne,

    /// <summary>Less than.</summary>
    Lt,

    /// <summary>At most.</summary>
    Le,

    /// <summary>Greater than.</summary>
    Gt,

    /// <summary>At least.</summary>
    Ge,
}

/// <summary>Sort direction.</summary>
public enum SortDirection
{
    /// <summary>Smallest first.</summary>
    Asc,

    /// <summary>Largest first.</summary>
    Desc,
}

/// <summary>The type of a report parameter's value.</summary>
public enum ParamType
{
    /// <summary>Text.</summary>
    Text,

    /// <summary>A whole number.</summary>
    Whole,

    /// <summary>An exact decimal.</summary>
    Exact,

    /// <summary>A calendar date.</summary>
    Day,

    /// <summary>A record id.</summary>
    Link,

    /// <summary>One of a listed set.</summary>
    Select,
}

/// <summary>A literal a filter compares with. Decimals are text, never floats.</summary>
public abstract record FilterValue
{
    /// <summary>Text.</summary>
    public sealed record Text(string Value) : FilterValue;

    /// <summary>A whole number.</summary>
    public sealed record Whole(long Value) : FilterValue;

    /// <summary>Yes or no.</summary>
    public sealed record Bool(bool Value) : FilterValue;

    /// <summary>An exact decimal, as text.</summary>
    public sealed record Exact(string Value) : FilterValue;

    /// <summary>A date written <c>YYYY-MM-DD</c>.</summary>
    public sealed record Day(string Value) : FilterValue;
}

/// <summary>The right side of a filter: a value the viewer supplies, or a fixed literal.</summary>
public abstract record FilterTarget
{
    /// <summary>A report parameter, by name.</summary>
    public sealed record Parameter(string Name) : FilterTarget;

    /// <summary>A fixed literal.</summary>
    public sealed record Literal(FilterValue Value) : FilterTarget;
}

/// <summary>One column of a report draft.</summary>
public sealed record DraftColumn(string Field, string? Label = null, Aggregate? Aggregate = null, string? Format = null)
{
    /// <summary>The output name: the field path with a dot replaced by an underscore.</summary>
    public string Name => Field.Replace('.', '_');
}

/// <summary>A link a draft follows.</summary>
public sealed record DraftJoin(string Alias, string DocType, string On);

/// <summary>A condition every row must meet.</summary>
public sealed record DraftFilter(string Field, Compare Op, FilterTarget Target);

/// <summary>A value the viewer may supply.</summary>
public sealed record DraftParameter(string Name, string Label, ParamType Type, bool Required = false, FilterValue? Default = null);

/// <summary>One sort key.</summary>
public sealed record DraftSort(string Field, SortDirection Direction);

/// <summary>The shape a chart draws.</summary>
public enum ChartKind
{
    /// <summary>One large number.</summary>
    Number,

    /// <summary>Bars.</summary>
    Bar,

    /// <summary>A line.</summary>
    Line,

    /// <summary>A pie.</summary>
    Pie,

    /// <summary>A plain table.</summary>
    Table,
}

/// <summary>The chart a report or tile draws, over the report's own columns.</summary>
public sealed record ChartSpec(ChartKind Kind, string? X, IReadOnlyList<string> Series, string? Title = null);

/// <summary>A report draft as the server holds it.</summary>
public sealed record ReportDraft(
    string Id,
    string Name,
    string Label,
    string Base,
    string Status,
    IReadOnlyList<DraftColumn> Columns,
    IReadOnlyList<DraftJoin> Joins,
    IReadOnlyList<DraftParameter> Parameters,
    IReadOnlyList<DraftFilter> Filters,
    IReadOnlyList<string> GroupBy,
    IReadOnlyList<DraftSort> Sort,
    IReadOnlyList<string> Totals,
    ChartSpec? Chart,
    IReadOnlyList<string> Roles);

/// <summary>A problem the server found, in plain language.</summary>
public sealed record BuilderIssue(string Code, bool IsError, string? Path, string Message, string Remedy)
{
    /// <summary>"Fix" or "Check", for the panel.</summary>
    public string SeverityText => IsError ? "Fix" : "Check";
}

/// <summary>What happened to one op: the changed draft, or the refusal.</summary>
public sealed record ReportOutcome(ReportDraft? Draft, long Version, BuilderIssue? Refusal)
{
    /// <summary>Whether the server applied the op.</summary>
    public bool Ok => Refusal is null && Draft is not null;
}

/// <summary>A column of a bounded preview.</summary>
public sealed record PreviewColumn(string Name, string Label, bool Masked);

/// <summary>A bounded preview or run: a row cap, and masked columns the viewer may not see.</summary>
public sealed record ReportRows(IReadOnlyList<PreviewColumn> Columns, IReadOnlyList<IReadOnlyList<string?>> Rows, bool Truncated, int Cap);

/// <summary>A published report a dashboard tile can draw on.</summary>
public sealed record ReportSummary(string Name, string Label, IReadOnlyList<string> Columns);

/// <summary>The result of a publish.</summary>
public sealed record PublishOutcome(bool Ok, string Message, IReadOnlyList<BuilderIssue> Issues, string? Reference = null);
