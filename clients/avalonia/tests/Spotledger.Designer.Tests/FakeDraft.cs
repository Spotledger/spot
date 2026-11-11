namespace Spotledger.Designer.Tests;

/// <summary>
/// The mutable state of one draft, as the in-memory server keeps it between ops: the field
/// definitions, the form layout, the child DocTypes and the DocType's own settings. It mirrors
/// the shape <c>spot-nocode-model</c> keeps (definition + layout + children) and nothing more; the
/// snapshot the UI sees is built from it by <see cref="FakeDesignerServer"/>.
/// </summary>
internal sealed class FakeDraft
{
    public string Id { get; init; } = "draft-vsr";

    public string Owner { get; init; } = "ann";

    public string Doctype { get; init; } = "vehicle_service_request";

    public string Label { get; init; } = "Vehicle Service Request";

    public string Status { get; set; } = "draft";

    public long Version { get; set; } = 1;

    public bool NewDocType { get; init; } = true;

    public List<FakeField> Fields { get; init; } = new();

    public List<FakeSection> Sections { get; init; } = new();

    public List<FakeTab> Tabs { get; init; } = new();

    public List<FakeChild> Children { get; init; } = new();

    /// <summary>Fields the design removed; the publish plan counts them as removals.</summary>
    public List<string> RemovedFields { get; init; } = new();

    public string PermissionsTemplate { get; set; } = "standard";

    public string? NamingSeries { get; set; }

    public FakeDraft Clone() => new()
    {
        Id = Id,
        Owner = Owner,
        Doctype = Doctype,
        Label = Label,
        Status = Status,
        Version = Version,
        NewDocType = NewDocType,
        Fields = Fields.Select(f => f.Clone()).ToList(),
        Sections = Sections.Select(s => s.Clone()).ToList(),
        Tabs = Tabs.Select(t => t.Clone()).ToList(),
        Children = Children.Select(c => c.Clone()).ToList(),
        RemovedFields = RemovedFields.ToList(),
        PermissionsTemplate = PermissionsTemplate,
        NamingSeries = NamingSeries,
    };

    public IReadOnlyList<FakeField> FieldsOf(string? child) =>
        child is null ? Fields : Child(child)?.Fields ?? (IReadOnlyList<FakeField>)Array.Empty<FakeField>();

    public List<FakeSection> SectionsOf(string? child) =>
        child is null ? Sections : Child(child)?.Sections ?? new List<FakeSection>();

    public FakeField? Field(string name, string? child) =>
        FieldsOf(child).FirstOrDefault(f => string.Equals(f.Name, name, StringComparison.Ordinal));

    public FakeChild? Child(string doctype) =>
        Children.FirstOrDefault(c => string.Equals(c.Doctype, doctype, StringComparison.Ordinal));

    public FakeSection? Section(string id, string? child) =>
        SectionsOf(child).FirstOrDefault(s => string.Equals(s.Id, id, StringComparison.Ordinal));

    /// <summary>Every field name the layout places, for the duplicate-placement guards.</summary>
    public IReadOnlyList<string> Placed(string? child) =>
        SectionsOf(child).SelectMany(s => s.Items).ToList();
}

/// <summary>One field of a fake draft.</summary>
internal sealed class FakeField
{
    public string Name { get; init; } = string.Empty;

    public string Label { get; init; } = string.Empty;

    public string Type { get; init; } = "text";

    public bool Required { get; set; }

    public string? Default { get; set; }

    public List<string> Options { get; init; } = new();

    public bool ReadOnly { get; init; }

    public bool Hidden { get; init; }

    public string? DependsOnText { get; set; }

    public string? ValidationText { get; init; }

    public string? LinkTarget { get; init; }

    public string? ChildDoctype { get; init; }

    public FakeField Clone() => new()
    {
        Name = Name,
        Label = Label,
        Type = Type,
        Required = Required,
        Default = Default,
        Options = Options.ToList(),
        ReadOnly = ReadOnly,
        Hidden = Hidden,
        DependsOnText = DependsOnText,
        ValidationText = ValidationText,
        LinkTarget = LinkTarget,
        ChildDoctype = ChildDoctype,
    };

    public FakeField With(string? name = null, string? type = null) => new()
    {
        Name = name ?? Name,
        Label = Label,
        Type = type ?? Type,
        Required = Required,
        Default = Default,
        Options = Options.ToList(),
        ReadOnly = ReadOnly,
        Hidden = Hidden,
        DependsOnText = DependsOnText,
        ValidationText = ValidationText,
        LinkTarget = LinkTarget,
        ChildDoctype = ChildDoctype,
    };
}

/// <summary>One form section of a fake draft.</summary>
internal sealed class FakeSection
{
    public string Id { get; init; } = string.Empty;

    public string Title { get; init; } = string.Empty;

    public int? Columns { get; set; }

    public string? TabId { get; init; }

    public List<string> Items { get; init; } = new();

    public FakeSection Clone() => new()
    {
        Id = Id,
        Title = Title,
        Columns = Columns,
        TabId = TabId,
        Items = Items.ToList(),
    };
}

/// <summary>One tab of a fake draft's form.</summary>
internal sealed class FakeTab
{
    public string Id { get; init; } = string.Empty;

    public string Title { get; init; } = string.Empty;

    public FakeTab Clone() => new() { Id = Id, Title = Title };
}

/// <summary>One child DocType of a fake draft.</summary>
internal sealed class FakeChild
{
    public string Doctype { get; init; } = string.Empty;

    public string Label { get; init; } = string.Empty;

    public List<FakeField> Fields { get; init; } = new();

    public List<FakeSection> Sections { get; init; } = new();

    /// <summary>One of the child's fields, by name.</summary>
    public FakeField? Field(string name) =>
        Fields.FirstOrDefault(f => string.Equals(f.Name, name, StringComparison.Ordinal));

    public FakeChild Clone() => new()
    {
        Doctype = Doctype,
        Label = Label,
        Fields = Fields.Select(f => f.Clone()).ToList(),
        Sections = Sections.Select(s => s.Clone()).ToList(),
    };
}
