namespace Spotledger.Client.Designer;

/// <summary>A field as the server's draft reports it.</summary>
public sealed record FieldSnapshot(
    string Name,
    string Label,
    string Type,
    bool Required = false,
    string? Default = null,
    IReadOnlyList<string>? Options = null,
    bool ReadOnly = false,
    bool Hidden = false,
    string? DependsOnText = null,
    string? ValidationText = null,
    string? LinkTarget = null,
    string? ChildDoctype = null)
{
    /// <summary>The select options, never null.</summary>
    public IReadOnlyList<string> OptionList => Options ?? Array.Empty<string>();
}

/// <summary>A form section: its id, title, column count, tab and the fields in order.</summary>
public sealed record SectionSnapshot(
    string Id,
    string Title,
    int? Columns = null,
    string? TabId = null,
    IReadOnlyList<string>? Items = null)
{
    /// <summary>The field names in the section, in order; never null.</summary>
    public IReadOnlyList<string> FieldItems => Items ?? Array.Empty<string>();
}

/// <summary>A form tab and the sections it holds.</summary>
public sealed record TabSnapshot(string Id, string Title);

/// <summary>A form layout: its tabs and its sections (top-level sections included).</summary>
public sealed record LayoutSnapshot(
    IReadOnlyList<TabSnapshot>? Tabs = null,
    IReadOnlyList<SectionSnapshot>? Sections = null)
{
    /// <summary>The layout's tabs; never null.</summary>
    public IReadOnlyList<TabSnapshot> TabList => Tabs ?? Array.Empty<TabSnapshot>();

    /// <summary>The layout's sections, in order; never null.</summary>
    public IReadOnlyList<SectionSnapshot> SectionList => Sections ?? Array.Empty<SectionSnapshot>();

    /// <summary>A section by id.</summary>
    public SectionSnapshot? Section(string id) => SectionList.FirstOrDefault(s => string.Equals(s.Id, id, StringComparison.Ordinal));

    /// <summary>Every field that appears in some section.</summary>
    public IReadOnlyList<string> PlacedFields() => SectionList.SelectMany(s => s.FieldItems).ToList();

    /// <summary>An empty layout.</summary>
    public static LayoutSnapshot Empty { get; } = new();
}

/// <summary>A child DocType of the draft with its own fields and grid layout.</summary>
public sealed record ChildSnapshot(
    string Doctype,
    string Label,
    IReadOnlyList<FieldSnapshot>? Fields = null,
    LayoutSnapshot? Layout = null)
{
    /// <summary>The child's fields; never null.</summary>
    public IReadOnlyList<FieldSnapshot> FieldList => Fields ?? Array.Empty<FieldSnapshot>();

    /// <summary>The child's layout; never null.</summary>
    public LayoutSnapshot Grid => Layout ?? LayoutSnapshot.Empty;
}

/// <summary>One entry of the server's op log, for the history list.</summary>
public sealed record HistoryEntry(int Index, string Op, string Summary, bool IsCurrent = false);

/// <summary>
/// The draft as the server reports it after an op, an undo or a load: the definition, the
/// layout, the children, the op-log position and what this designer may do with it.
/// </summary>
public sealed record DraftDocument(
    string Id,
    string Owner,
    string Doctype,
    string Label,
    string Status,
    long Version,
    IReadOnlyList<FieldSnapshot>? Fields = null,
    IReadOnlyList<ChildSnapshot>? Children = null,
    LayoutSnapshot? Layout = null,
    string PermissionsTemplate = "standard",
    IReadOnlyList<HistoryEntry>? History = null,
    int UndoDepth = 0,
    int RedoDepth = 0,
    bool CanPublish = false,
    string? NamingSeries = null,
    IReadOnlyList<string>? SupportedOps = null,
    bool NewDocType = true)
{
    /// <summary>The root DocType's fields, in definition order; never null.</summary>
    public IReadOnlyList<FieldSnapshot> FieldList => Fields ?? Array.Empty<FieldSnapshot>();

    /// <summary>The draft's child DocTypes; never null.</summary>
    public IReadOnlyList<ChildSnapshot> ChildList => Children ?? Array.Empty<ChildSnapshot>();

    /// <summary>The form layout; never null.</summary>
    public LayoutSnapshot Form => Layout ?? LayoutSnapshot.Empty;

    /// <summary>The server's op log as the history list; never null.</summary>
    public IReadOnlyList<HistoryEntry> Log => History ?? Array.Empty<HistoryEntry>();

    /// <summary>The op names this server's draft model accepts (capability gate for the inspector).</summary>
    public IReadOnlyList<string> Ops => SupportedOps ?? Array.Empty<string>();

    /// <summary>A root field by name.</summary>
    public FieldSnapshot? Field(string name) =>
        FieldList.FirstOrDefault(f => string.Equals(f.Name, name, StringComparison.Ordinal));

    /// <summary>A child DocType by name.</summary>
    public ChildSnapshot? ChildDoc(string name) =>
        ChildList.FirstOrDefault(c => string.Equals(c.Doctype, name, StringComparison.Ordinal));

    /// <summary>The fields of the root or a child DocType.</summary>
    public IReadOnlyList<FieldSnapshot> FieldsOf(string? childDoctype) =>
        childDoctype is null ? FieldList : ChildDoc(childDoctype)?.FieldList ?? Array.Empty<FieldSnapshot>();

    /// <summary>The layout of the root or a child DocType.</summary>
    public LayoutSnapshot LayoutOf(string? childDoctype) =>
        childDoctype is null ? Form : ChildDoc(childDoctype)?.Grid ?? LayoutSnapshot.Empty;

    /// <summary>Fields of a DocType that no section places yet (the "not on the form" tray).</summary>
    public IReadOnlyList<FieldSnapshot> UnplacedFields(string? childDoctype)
    {
        var placed = LayoutOf(childDoctype).PlacedFields();
        return FieldsOf(childDoctype)
            .Where(f => !placed.Contains(f.Name, StringComparer.Ordinal))
            .ToList();
    }

    /// <summary>Whether the server's draft model accepts an op name.</summary>
    public bool Supports(string op) => Ops.Contains(op, StringComparer.Ordinal);
}

/// <summary>How serious a server validation issue is.</summary>
public enum IssueSeverity
{
    /// <summary>The design cannot publish until it is fixed.</summary>
    Error,

    /// <summary>The design publishes, but something probably is not what the designer meant.</summary>
    Warning,
}

/// <summary>
/// A server validation issue in plain language, with the element it is about so the panel can
/// select it. The message and the suggestion are the server's words; the client only shows them.
/// </summary>
public sealed record ValidationIssue(
    string Code,
    string SourceCode,
    string Message,
    string Suggestion,
    IssueSeverity Severity,
    string DocType,
    string? Field = null,
    string? Section = null)
{
    /// <summary>The element path in the server's form (`doctype` or `doctype.field` or `doctype#section`).</summary>
    public string Path => Section is not null ? $"{DocType}#{Section}" : Field is not null ? $"{DocType}.{Field}" : DocType;

    /// <summary>A person-readable severity.</summary>
    public string SeverityText => Severity == IssueSeverity.Error ? "Must fix" : "Check";
}

/// <summary>The server's validation report for the draft at a version.</summary>
public sealed record ValidationReportSnapshot(long Version, IReadOnlyList<ValidationIssue>? Issues = null)
{
    /// <summary>The issues, never null.</summary>
    public IReadOnlyList<ValidationIssue> IssueList => Issues ?? Array.Empty<ValidationIssue>();

    /// <summary>Whether the design may publish.</summary>
    public bool IsOk => IssueList.All(i => i.Severity != IssueSeverity.Error);
}

/// <summary>What the outcome of sending an op to the server was.</summary>
public sealed record OpOutcome(DraftDocument? Draft = null, ValidationIssue? Issue = null)
{
    /// <summary>Whether the server applied the op.</summary>
    public bool Accepted => Issue is null && Draft is not null;
}

/// <summary>A field of the preview, with the sample value the preview shows.</summary>
public sealed record PreviewFieldSnapshot(
    string Name,
    string Label,
    string Type,
    bool Required = false,
    string? Sample = null,
    IReadOnlyList<string>? Options = null,
    string? LinkTarget = null,
    string? ChildDoctype = null);

/// <summary>The preview of a child DocType: the grid the table field renders.</summary>
public sealed record PreviewChildSnapshot(
    string Doctype,
    string Label,
    IReadOnlyList<PreviewFieldSnapshot>? Fields = null,
    LayoutSnapshot? Grid = null);

/// <summary>
/// The form as the server previews it: an ephemeral metadata overlay (never persisted) and the
/// layout the real form renders, plus sample data.
/// </summary>
public sealed record PreviewModel(
    string Doctype,
    string Label,
    long Version,
    LayoutSnapshot? Form = null,
    LayoutSnapshot? List = null,
    IReadOnlyList<PreviewFieldSnapshot>? Fields = null,
    IReadOnlyList<PreviewChildSnapshot>? Children = null,
    bool Sandbox = true,
    bool Valid = false,
    IReadOnlyList<ValidationIssue>? Issues = null)
{
    /// <summary>The previewed fields, never null.</summary>
    public IReadOnlyList<PreviewFieldSnapshot> FieldList => Fields ?? Array.Empty<PreviewFieldSnapshot>();

    /// <summary>The previewed children, never null.</summary>
    public IReadOnlyList<PreviewChildSnapshot> ChildList => Children ?? Array.Empty<PreviewChildSnapshot>();

    /// <summary>The previewed issues, never null.</summary>
    public IReadOnlyList<ValidationIssue> IssueList => Issues ?? Array.Empty<ValidationIssue>();

    /// <summary>The form layout, never null.</summary>
    public LayoutSnapshot FormLayout => Form ?? LayoutSnapshot.Empty;

    /// <summary>A previewed field by name.</summary>
    public PreviewFieldSnapshot? Field(string name) =>
        FieldList.FirstOrDefault(f => string.Equals(f.Name, name, StringComparison.Ordinal));
}

/// <summary>Which layout the preview pane shows.</summary>
public enum PreviewLayout
{
    /// <summary>The form at desktop width.</summary>
    Desktop,

    /// <summary>The form at a narrow width (the same sections, one column).</summary>
    Narrow,

    /// <summary>The list view of the DocType.</summary>
    List,
}

/// <summary>How risky applying a design is, in the migration plan's own vocabulary.</summary>
public enum PublishRisk
{
    /// <summary>Adds things; no existing data changes.</summary>
    Additive,

    /// <summary>Changes data in place; the plan describes what happens to it.</summary>
    InPlace,

    /// <summary>Needs a maintenance window.</summary>
    Blocking,
}

/// <summary>A readable description of one change the publish will apply.</summary>
public sealed record PublishChange(string Kind, string Summary, bool Removes = false);

/// <summary>
/// The server's publish plan summary: whether it can go ahead, how risky it is, what it
/// changes, and the confirmation the designer must give.
/// </summary>
public sealed record PublishPlanSummary(
    string DraftId,
    string Doctype,
    bool Ready,
    bool NewDoctype,
    PublishRisk Risk,
    IReadOnlyList<PublishChange>? Changes = null,
    IReadOnlyList<ValidationIssue>? Issues = null,
    int RecordCount = 0,
    string? ConfirmationToken = null,
    string? PlanId = null)
{
    /// <summary>The changes, never null.</summary>
    public IReadOnlyList<PublishChange> ChangeList => Changes ?? Array.Empty<PublishChange>();

    /// <summary>The blocking issues, never null.</summary>
    public IReadOnlyList<ValidationIssue> IssueList => Issues ?? Array.Empty<ValidationIssue>();

    /// <summary>The risk in a person's words.</summary>
    public string RiskText => Risk switch
    {
        PublishRisk.Additive => "Safe: adds to the site and changes nothing that exists",
        PublishRisk.InPlace => "Changes existing data in place",
        PublishRisk.Blocking => "Needs a maintenance window",
        _ => "Unknown",
    };
}

/// <summary>The server's receipt after a publish.</summary>
public sealed record PublishReceipt(string Doctype, long Version, int RecordsWritten, PublishRisk Risk);

/// <summary>How a type change would treat the values already stored.</summary>
public enum TypeChangeClassification
{
    /// <summary>Every existing value converts exactly.</summary>
    Safe,

    /// <summary>Some values would be rounded, truncated or reinterpreted.</summary>
    Lossy,

    /// <summary>The change is not allowed at all.</summary>
    Refused,
}

/// <summary>The server's classification of one type change, with what happens to the values.</summary>
public sealed record TypeChangePlanSummary(
    string Field,
    string From,
    string To,
    TypeChangeClassification Classification,
    string Summary,
    IReadOnlyList<string>? Details = null,
    SampleStats? Stats = null)
{
    /// <summary>The extra detail lines, never null.</summary>
    public IReadOnlyList<string> DetailList => Details ?? Array.Empty<string>();

    /// <summary>The classification in a person's words.</summary>
    public string ClassificationText => Classification switch
    {
        TypeChangeClassification.Safe => "Safe: every value converts exactly",
        TypeChangeClassification.Lossy => "Lossy: some values change",
        TypeChangeClassification.Refused => "Not allowed",
        _ => "Unknown",
    };

    /// <summary>Whether the type change may be applied.</summary>
    public bool CanApply => Classification != TypeChangeClassification.Refused;
}

/// <summary>A DocType the link picker can offer.</summary>
public sealed record DocTypeChoice(string Name, string Label, bool IsChild = false)
{
    /// <summary>"Label (name)" for the picker's rows.</summary>
    public override string ToString() => string.Equals(Label, Name, StringComparison.Ordinal) ? Name : $"{Label} ({Name})";
}

/// <summary>What went wrong with a designer command, in the designer's words but the server's code.</summary>
public sealed record DesignerResult(bool Ok, string? Code = null, string? Message = null, string? Suggestion = null, ValidationIssue? Issue = null)
{
    /// <summary>A successful result.</summary>
    public static DesignerResult Success { get; } = new(true);

    /// <summary>A refused result from a server issue.</summary>
    public static DesignerResult From(ValidationIssue issue) =>
        new(false, issue.Code, issue.Message, issue.Suggestion, issue);
}

/// <summary>What the designer's autosave last did.</summary>
public enum SaveState
{
    /// <summary>Every accepted op is on the server.</summary>
    Saved,

    /// <summary>An op was accepted and its autosave is in flight.</summary>
    Saving,

    /// <summary>The last autosave failed; the next accepted op tries again.</summary>
    Failed,
}

/// <summary>The permissions templates a design can choose, with what each one grants.</summary>
public sealed record PermissionsTemplateInfo(string Id, string Title, string Read, string Write, string Submit)
{
    /// <summary>The template's title, for the picker's rows.</summary>
    public override string ToString() => Title;
}

/// <summary>The templates the picker offers; the ids are the server's own template names.</summary>
public static class PermissionsTemplates
{
    /// <summary>Every template, in the order the picker shows them.</summary>
    public static IReadOnlyList<PermissionsTemplateInfo> All { get; } = new[]
    {
        new PermissionsTemplateInfo("standard", "Standard", "The creator's role and administrators", "The creator's role and administrators", "The creator's role and administrators"),
        new PermissionsTemplateInfo("read_only", "Everyone reads", "Everyone with access to the site", "Designers and administrators", "Designers and administrators"),
        new PermissionsTemplateInfo("private", "Private", "The designer and administrators", "The designer and administrators", "The designer and administrators"),
    };

    /// <summary>A template by id, or null.</summary>
    public static PermissionsTemplateInfo? Find(string id) =>
        All.FirstOrDefault(t => string.Equals(t.Id, id, StringComparison.Ordinal));
}
