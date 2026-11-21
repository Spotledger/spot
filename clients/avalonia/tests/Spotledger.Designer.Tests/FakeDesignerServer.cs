using System.Globalization;
using Spotledger.Client.Designer;

namespace Spotledger.Designer.Tests;

/// <summary>
/// The in-memory server the designer's tests run against: it applies the same op vocabulary
/// <c>spot-nocode-model</c> defines, keeps an op log with an undo/redo cursor, validates, previews
/// and plans a publish, and it is the only authority in these tests.
/// </summary>
/// <remarks>
/// It exists so the tests prove the commands the UI issues — the op stream, the refusals, the
/// publish gate — instead of re-testing the client's view of a design. It is deliberately strict
/// where the server is strict (names, duplicates, layout anchors, the data-loss token, the publish
/// permission) and permissive nowhere the real server decides.
/// </remarks>
internal sealed class FakeDesignerServer : IDesignerServer
{
    private readonly List<FakeDraft> _states = new();
    private readonly List<(string Op, string Summary)> _log = new();
    private int _cursor;

    /// <summary>Creates the server over an initial draft.</summary>
    public FakeDesignerServer(FakeDraft initial)
    {
        _states.Add(initial.Clone());
        CanPublish = true;
    }

    /// <summary>Whether this session holds the designer's publish permission.</summary>
    public bool CanPublish { get; set; }

    /// <summary>The versions autosave has been asked to save, in order.</summary>
    public List<long> Saves { get; } = new();

    /// <summary>How many previews were built.</summary>
    public int PreviewCount { get; private set; }

    /// <summary>How many publishes were performed.</summary>
    public int PublishCount { get; private set; }

    /// <summary>How many discards happened.</summary>
    public int DiscardCount { get; private set; }

    /// <summary>The current draft state the server holds.</summary>
    public FakeDraft Current => _states[_cursor];

    /// <summary>A fresh, empty 'Vehicle Service Request' draft.</summary>
    public static FakeDraft NewVehicleDraft() => new();

    /// <summary>A site with one installed DocType the golden design links to.</summary>
    public static IReadOnlyList<DocTypeChoice> SiteDoctypes { get; } = new[]
    {
        new DocTypeChoice("vehicle", "Vehicle"),
        new DocTypeChoice("customer", "Customer"),
        new DocTypeChoice("vehicle_service_item", "Vehicle Service Item", IsChild: true),
    };

    /// <inheritdoc />
    public Task<DraftDocument> OpenAsync(string draftId, CancellationToken cancellationToken = default) =>
        Task.FromResult(Snapshot());

    /// <inheritdoc />
    public Task<OpOutcome> ApplyAsync(string draftId, DesignerOp op, long baseVersion, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(op);
        var working = Current.Clone();
        var issue = Apply(working, op);
        if (issue is not null)
        {
            return Task.FromResult(new OpOutcome(null, issue));
        }

        working.Version = Current.Version + 1;
        while (_states.Count > _cursor + 1)
        {
            _states.RemoveAt(_states.Count - 1);
            _log.RemoveAt(_log.Count - 1);
        }

        _states.Add(working);
        _log.Add((op.Op, DesignerOpText.Describe(op)));
        _cursor++;
        return Task.FromResult(new OpOutcome(Snapshot(), null));
    }

    /// <inheritdoc />
    public Task<DraftDocument> UndoAsync(string draftId, CancellationToken cancellationToken = default)
    {
        if (_cursor > 0)
        {
            _cursor--;
        }

        return Task.FromResult(Snapshot());
    }

    /// <inheritdoc />
    public Task<DraftDocument> RedoAsync(string draftId, CancellationToken cancellationToken = default)
    {
        if (_cursor < _states.Count - 1)
        {
            _cursor++;
        }

        return Task.FromResult(Snapshot());
    }

    /// <inheritdoc />
    public Task SaveAsync(string draftId, long version, CancellationToken cancellationToken = default)
    {
        if (version > Current.Version)
        {
            throw new InvalidOperationException($"The draft is at version {Current.Version}, not {version}.");
        }

        Saves.Add(version);
        return Task.CompletedTask;
    }

    /// <inheritdoc />
    public Task<ValidationReportSnapshot> ValidateAsync(string draftId, long version, CancellationToken cancellationToken = default) =>
        Task.FromResult(Report());

    /// <inheritdoc />
    public Task<(string? Text, string? Error)> PreviewConditionAsync(string draftId, ConditionExpression condition, CancellationToken cancellationToken = default)
    {
        var text = Compile(condition, Current, null, out var error);
        return Task.FromResult((text, error));
    }

    /// <inheritdoc />
    public Task<TypeChangePlanSummary> PlanTypeChangeAsync(string draftId, string field, string newType, string? childDoctype = null, CancellationToken cancellationToken = default)
    {
        var snapshot = Current.Field(field, childDoctype);
        var from = snapshot?.Type ?? "text";
        var stats = StatsOf(snapshot);
        if (newType is "table" || from is "table")
        {
            var refusal = new List<string> { "The rows would need their own DocType." };
            return Task.FromResult(new TypeChangePlanSummary(
                field, from, newType, TypeChangeClassification.Refused,
                "A child table is a layout and storage change, not a field type: add a new child table instead.",
                refusal, stats));
        }

        if (from is "text" or "long_text" && newType is "int" or "decimal")
        {
            var unparsable = stats.Unparsable;
            var details = new List<string> { $"Checked {stats.Rows} stored values." };
            if (unparsable > 0)
            {
                details.Add($"{unparsable} would be emptied.");
            }

            return Task.FromResult(new TypeChangePlanSummary(
                field, from, newType,
                unparsable == 0 ? TypeChangeClassification.Safe : TypeChangeClassification.Lossy,
                unparsable == 0
                    ? "Every stored value is a whole number, so it converts exactly."
                    : $"{unparsable} of {stats.Rows} stored values are not numbers and would become empty.",
                details,
                stats));
        }

        var checkedValues = new List<string> { $"Checked {stats.Rows} stored values." };
        return Task.FromResult(new TypeChangePlanSummary(
            field, from, newType, TypeChangeClassification.Safe,
            "Every stored value converts exactly.",
            checkedValues, stats));
    }

    /// <inheritdoc />
    public Task<PreviewModel> PreviewAsync(string draftId, PreviewLayout layout, CancellationToken cancellationToken = default)
    {
        PreviewCount++;
        var draft = Current;
        var report = Report();
        var form = LayoutOf(draft, null, layout);
        var fields = draft.Fields.Select(f => PreviewOf(draft, f)).ToList();
        var children = draft.Children
            .Select(child => new PreviewChildSnapshot(
                child.Doctype,
                child.Label,
                child.Fields.Select(f => PreviewOf(draft, f)).ToList(),
                LayoutOf(draft, child, layout)))
            .ToList();
        var list = layout == PreviewLayout.List
            ? new LayoutSnapshot(
                Array.Empty<TabSnapshot>(),
                new[]
                {
                    new SectionSnapshot("list", "List view", 4, null,
                        draft.Fields.Take(4).Select(f => f.Name).ToList()),
                })
            : null;
        return Task.FromResult(new PreviewModel(
            draft.Doctype,
            draft.Label,
            draft.Version,
            form,
            list ?? LayoutOf(draft, null, PreviewLayout.List),
            fields,
            children,
            Sandbox: true,
            Valid: report.IsOk,
            report.IssueList));
    }

    /// <inheritdoc />
    public Task<PublishPlanSummary> PlanPublishAsync(string draftId, CancellationToken cancellationToken = default)
    {
        var draft = Current;
        var report = Report();
        var changes = new List<PublishChange>
        {
            new("doctype", draft.NewDocType
                ? $"Create the DocType {draft.Label} ({draft.Doctype})."
                : $"Update the DocType {draft.Label} ({draft.Doctype}).", false),
        };
        foreach (var field in draft.Fields)
        {
            changes.Add(new PublishChange("field", $"Add the {FieldTypeCatalog.TitleOf(field.Type).ToLowerInvariant()} field {field.Label} ({field.Name})."));
        }

        foreach (var section in draft.Sections)
        {
            changes.Add(new PublishChange("section", $"Add the form section {section.Title}."));
        }

        foreach (var child in draft.Children)
        {
            changes.Add(new PublishChange("child", $"Create the child DocType {child.Label} ({child.Doctype})."));
        }

        foreach (var removed in draft.RemovedFields)
        {
            changes.Add(new PublishChange("field", $"Drop the field {removed} and its stored values.", true));
        }

        var risk = draft.NewDocType
            ? PublishRisk.Additive
            : draft.RemovedFields.Count > 0 ? PublishRisk.Blocking : PublishRisk.InPlace;
        var ready = report.IsOk && !string.Equals(draft.Status, "discarded", StringComparison.Ordinal);
        return Task.FromResult(new PublishPlanSummary(
            draft.Id,
            draft.Doctype,
            ready,
            draft.NewDocType,
            risk,
            changes,
            report.IssueList,
            RecordCount: draft.NewDocType ? 0 : 12,
            ConfirmationToken: $"publish {draft.Doctype} v{draft.Version}",
            PlanId: $"plan-{draft.Version}"));
    }

    /// <inheritdoc />
    public Task<PublishReceipt> PublishAsync(string draftId, string planId, CancellationToken cancellationToken = default)
    {
        var draft = Current;
        if (string.Equals(draft.Status, "discarded", StringComparison.Ordinal))
        {
            throw new DesignerRefusalException(Issue("nocode_model.not_ready", "This draft was discarded and can never publish.", "Start a new design instead.", IssueSeverity.Error, draft.Doctype));
        }

        if (!CanPublish)
        {
            throw new DesignerRefusalException(Issue(
                "nocode_model.publish_forbidden",
                "You do not have permission to publish designs on this site.",
                "Ask an administrator for the designer publish permission.",
                IssueSeverity.Error,
                draft.Doctype));
        }

        if (!Report().IsOk)
        {
            throw new DesignerRefusalException(Issue("nocode_model.not_ready", "This design cannot publish yet.", "Fix the issues the validation panel lists.", IssueSeverity.Error, draft.Doctype));
        }

        PublishCount++;
        var next = draft.Clone();
        next.Status = "published";
        next.Version = draft.Version + 1;
        _states[_cursor] = next;
        return Task.FromResult(new PublishReceipt(next.Doctype, next.Version, 1, PublishRisk.Additive));
    }

    /// <inheritdoc />
    public Task DiscardAsync(string draftId, CancellationToken cancellationToken = default)
    {
        DiscardCount++;
        var next = Current.Clone();
        next.Status = "discarded";
        next.Version = Current.Version + 1;
        _states[_cursor] = next;
        return Task.CompletedTask;
    }

    /// <inheritdoc />
    public Task<IReadOnlyList<DocTypeChoice>> SearchDocTypesAsync(string draftId, string query, CancellationToken cancellationToken = default)
    {
        var normalized = query?.Trim() ?? string.Empty;
        var found = SiteDoctypes
            .Where(choice => normalized.Length == 0
                || choice.Name.Contains(normalized, StringComparison.OrdinalIgnoreCase)
                || choice.Label.Contains(normalized, StringComparison.OrdinalIgnoreCase))
            .ToList();
        return Task.FromResult<IReadOnlyList<DocTypeChoice>>(found);
    }

    // ---------------------------------------------------------------- the draft as the UI sees it

    private DraftDocument Snapshot()
    {
        var draft = Current;
        var issue = Report();
        var history = _log
            .Select((entry, index) => new HistoryEntry(index + 1, entry.Op, entry.Summary, index == _cursor - 1))
            .ToList();
        return new DraftDocument(
            draft.Id,
            draft.Owner,
            draft.Doctype,
            draft.Label,
            draft.Status,
            draft.Version,
            draft.Fields.Select(FieldSnapshotOf).ToList(),
            draft.Children
                .Select(child => new ChildSnapshot(
                    child.Doctype,
                    child.Label,
                    child.Fields.Select(FieldSnapshotOf).ToList(),
                    LayoutOf(draft, child, PreviewLayout.Desktop)))
                .ToList(),
            LayoutOf(draft, null, PreviewLayout.Desktop),
            draft.PermissionsTemplate,
            history,
            _cursor,
            _states.Count - 1 - _cursor,
            CanPublish,
            draft.NamingSeries,
            Ops,
            draft.NewDocType);
    }

    private static readonly string[] Ops =
    {
        "add_field", "rename_field", "remove_field", "change_type", "set_options", "set_required",
        "set_default", "reorder_field", "add_child_table", "add_link", "set_naming_series",
        "set_permissions_template", "add_form_section", "add_form_tab", "set_section_columns",
        "move_field_in_layout", "set_depends_on",
    };

    private static FieldSnapshot FieldSnapshotOf(FakeField field) => new(
        field.Name,
        field.Label,
        field.Type,
        field.Required,
        field.Default,
        field.Options.ToList(),
        field.ReadOnly,
        field.Hidden,
        field.DependsOnText,
        field.ValidationText,
        field.LinkTarget,
        field.ChildDoctype);

    private static LayoutSnapshot LayoutOf(FakeDraft draft, FakeChild? child, PreviewLayout layout)
    {
        var sections = child is null ? draft.Sections : child.Sections;
        var tabs = child is null
            ? draft.Tabs.Select(t => new TabSnapshot(t.Id, t.Title)).ToList()
            : new List<TabSnapshot>();
        var snapshots = sections.Select(section => new SectionSnapshot(
            section.Id,
            section.Title,
            layout == PreviewLayout.Narrow ? 1 : section.Columns,
            section.TabId,
            section.Items.ToList())).ToList();
        return new LayoutSnapshot(tabs, snapshots);
    }

    private static PreviewFieldSnapshot PreviewOf(FakeDraft draft, FakeField field)
    {
        var sample = field.Type switch
        {
            "text" => $"{field.Label} sample",
            "long_text" => $"A longer sample for {field.Label}.",
            "int" => "3",
            "decimal" => "1250.00",
            "date" => "2026-10-19",
            "time" => "09:30:00",
            "datetime" => "2026-10-19T09:30:00",
            "bool" => "true",
            "select" => field.Options.FirstOrDefault(),
            "link" => "VH-001",
            _ => null,
        };
        return new PreviewFieldSnapshot(
            field.Name,
            field.Label,
            field.Type,
            field.Required,
            sample,
            field.Options.ToList(),
            field.LinkTarget,
            field.ChildDoctype);
    }

    // ---------------------------------------------------------------- validation

    private ValidationReportSnapshot Report()
    {
        var draft = Current;
        var issues = new List<ValidationIssue>();
        if (draft.Fields.Count == 0 && draft.Children.Count == 0)
        {
            issues.Add(Issue("nocode_model.bad_layout", "The DocType has no fields yet.", "Add a field from the palette to begin.", IssueSeverity.Error, draft.Doctype));
        }

        foreach (var field in draft.Fields.Concat(draft.Children.SelectMany(c => c.Fields)))
        {
            if (!DesignerNaming.IsStorageName(field.Name))
            {
                issues.Add(Issue("nocode_model.bad_name", $"\"{field.Name}\" is not a valid field name.", "Use lower-case letters, digits and underscores.", IssueSeverity.Error, draft.Doctype, field.Name, null));
            }

            if (field.Type == "select" && field.Options.Count == 0)
            {
                issues.Add(Issue(
                    "nocode_model.bad_options",
                    $"The choice list \"{field.Label}\" has no values to choose from.",
                    "Add at least one value, or make the field a text field.",
                    IssueSeverity.Error,
                    draft.Doctype,
                    field.Name,
                    null));
            }

            if (field.Type == "table" && draft.Child(field.ChildDoctype ?? string.Empty) is { Fields.Count: 0 })
            {
                issues.Add(Issue(
                    "nocode_model.bad_layout",
                    $"The child table \"{field.Label}\" has no columns yet.",
                    "Open the grid and add the fields a row needs.",
                    IssueSeverity.Error,
                    draft.Doctype,
                    field.Name,
                    null));
            }
        }

        foreach (var section in draft.Sections)
        {
            if (section.Items.Count == 0)
            {
                issues.Add(Issue(
                    "nocode_model.bad_layout",
                    $"The section \"{section.Title}\" is empty.",
                    "Drop a field into it, or delete the section.",
                    IssueSeverity.Warning,
                    draft.Doctype,
                    null,
                    section.Id));
            }
        }

        var placed = draft.Placed(null);
        foreach (var field in draft.Fields.Where(f => !placed.Contains(f.Name, StringComparer.Ordinal)))
        {
            issues.Add(Issue(
                "nocode_model.bad_layout",
                $"The field \"{field.Label}\" is not on the form yet.",
                "Drag it onto a section so people can fill it in.",
                IssueSeverity.Warning,
                draft.Doctype,
                field.Name,
                null));
        }

        return new ValidationReportSnapshot(draft.Version, issues);
    }

    private static ValidationIssue Issue(string code, string message, string suggestion, IssueSeverity severity, string doctype, string? field = null, string? section = null) =>
        new(code, code, message, suggestion, severity, doctype, field, section);

    // ---------------------------------------------------------------- op application

    private ValidationIssue? Apply(FakeDraft draft, DesignerOp op)
    {
        switch (op)
        {
            case AddFieldOp add:
            {
                if (!DesignerNaming.IsStorageName(add.Name))
                {
                    return Refuse("nocode_model.bad_name", $"\"{add.Name}\" is not a valid field name.", "Use lower-case letters, digits and underscores, starting with a letter.", "nocode_model.bad_name");
                }

                if (draft.Field(add.Name, add.ChildDoctype) is not null)
                {
                    return Refuse("nocode_model.duplicate_field", $"\"{add.Name}\" already exists in this DocType.", "Pick another name.", add.ChildDoctype ?? draft.Doctype, add.Name);
                }

                var fields = FieldsOfMutable(draft, add.ChildDoctype);
                if (fields is null)
                {
                    return Refuse("nocode_model.unknown_child", $"\"{add.ChildDoctype}\" is not a child DocType of this draft.", "Add the child table first.", draft.Doctype);
                }

                var field = new FakeField { Name = add.Name, Label = add.Label, Type = add.Type };
                InsertField(fields, field, add.InsertAfter);
                return null;
            }

            case RenameFieldOp rename:
            {
                var fields = FieldsOfMutable(draft, rename.ChildDoctype);
                var field = fields?.FirstOrDefault(f => f.Name == rename.Name);
                if (fields is null || field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{rename.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, rename.Name);
                }

                if (!DesignerNaming.IsStorageName(rename.To))
                {
                    return Refuse("nocode_model.bad_name", $"\"{rename.To}\" is not a valid field name.", "Use lower-case letters, digits and underscores.", draft.Doctype, rename.Name);
                }

                if (fields.Any(f => f.Name == rename.To))
                {
                    return Refuse("nocode_model.duplicate_field", $"\"{rename.To}\" already exists in this DocType.", "Pick another name.", draft.Doctype, rename.Name);
                }

                var renamed = field.With(name: rename.To);
                fields[fields.IndexOf(field)] = renamed;
                foreach (var section in draft.SectionsOf(rename.ChildDoctype))
                {
                    for (var i = 0; i < section.Items.Count; i++)
                    {
                        if (section.Items[i] == rename.Name)
                        {
                            section.Items[i] = rename.To;
                        }
                    }
                }

                return null;
            }

            case RemoveFieldOp remove:
            {
                var fields = FieldsOfMutable(draft, remove.ChildDoctype);
                var field = fields?.FirstOrDefault(f => f.Name == remove.Name);
                if (fields is null || field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{remove.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, remove.Name);
                }

                var token = DesignerOpText.DataLossToken(remove.ChildDoctype ?? draft.Doctype, remove.Name);
                if (!string.Equals(remove.Confirmation, token, StringComparison.Ordinal))
                {
                    return Refuse("nocode_model.needs_data_loss_confirmation", $"Removing \"{remove.Name}\" deletes the values stored in it.", $"Send the confirmation \"{token}\" to continue.", draft.Doctype, remove.Name);
                }

                fields.Remove(field);
                foreach (var section in draft.SectionsOf(remove.ChildDoctype))
                {
                    section.Items.RemoveAll(name => name == remove.Name);
                }

                if (remove.ChildDoctype is null)
                {
                    draft.RemovedFields.Add(remove.Name);
                }

                return null;
            }

            case ChangeTypeOp change:
            {
                var fields = FieldsOfMutable(draft, change.ChildDoctype);
                var field = fields?.FirstOrDefault(f => f.Name == change.Name);
                if (fields is null || field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{change.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, change.Name);
                }

                if (FieldTypeCatalog.Find(change.Type) is null)
                {
                    return Refuse("nocode_model.bad_target", $"\"{change.Type}\" is not a field type this server knows.", "Pick a type from the palette.", draft.Doctype, change.Name);
                }

                fields[fields.IndexOf(field)] = field.With(type: change.Type);
                return null;
            }

            case SetOptionsOp options:
            {
                var field = draft.Field(options.Name, options.ChildDoctype);
                if (field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{options.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, options.Name);
                }

                var values = options.Options.Where(o => !string.IsNullOrWhiteSpace(o)).Select(o => o.Trim()).ToList();
                if (values.Count != values.Distinct(StringComparer.Ordinal).Count())
                {
                    return Refuse("nocode_model.bad_options", "The choice list has the same value twice.", "Remove the duplicate.", draft.Doctype, options.Name);
                }

                field.Options.Clear();
                field.Options.AddRange(values);
                return null;
            }

            case SetRequiredOp required:
            {
                var field = draft.Field(required.Name, required.ChildDoctype);
                if (field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{required.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, required.Name);
                }

                field.Required = required.Required;
                return null;
            }

            case SetDefaultOp defaults:
            {
                var field = draft.Field(defaults.Name, defaults.ChildDoctype);
                if (field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{defaults.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, defaults.Name);
                }

                field.Default = defaults.Default;
                return null;
            }

            case ReorderFieldOp reorder:
            {
                var fields = FieldsOfMutable(draft, reorder.ChildDoctype);
                var field = fields?.FirstOrDefault(f => f.Name == reorder.Name);
                if (fields is null || field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{reorder.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, reorder.Name);
                }

                fields.Remove(field);
                InsertField(fields, field, reorder.After);
                return null;
            }

            case AddChildTableOp table:
            {
                if (draft.Field(table.Name, null) is not null)
                {
                    return Refuse("nocode_model.duplicate_field", $"\"{table.Name}\" already exists in this DocType.", "Pick another name.", draft.Doctype, table.Name);
                }

                if (draft.Child(table.ChildDoctypeName) is not null)
                {
                    return Refuse("nocode_model.duplicate_field", $"\"{table.ChildDoctypeName}\" already exists as a child DocType.", "Pick another name.", draft.Doctype, table.Name);
                }

                InsertField(draft.Fields, new FakeField
                {
                    Name = table.Name,
                    Label = table.Label,
                    Type = "table",
                    ChildDoctype = table.ChildDoctypeName,
                }, table.InsertAfter);
                draft.Children.Add(new FakeChild { Doctype = table.ChildDoctypeName, Label = table.ChildTitle });
                return null;
            }

            case AddLinkOp link:
            {
                if (draft.Field(link.Name, null) is not null)
                {
                    return Refuse("nocode_model.duplicate_field", $"\"{link.Name}\" already exists in this DocType.", "Pick another name.", draft.Doctype, link.Name);
                }

                if (string.IsNullOrWhiteSpace(link.Target))
                {
                    return Refuse("nocode_model.bad_target", "A link needs the DocType it points at.", "Choose the DocType in the picker.", draft.Doctype, link.Name);
                }

                InsertField(draft.Fields, new FakeField
                {
                    Name = link.Name,
                    Label = link.Label,
                    Type = "link",
                    LinkTarget = link.Target,
                }, link.InsertAfter);
                return null;
            }

            case SetNamingSeriesOp naming:
            {
                if (naming.Naming is { Length: > 0 } pattern && !pattern.Contains('{', StringComparison.Ordinal))
                {
                    return Refuse("nocode_model.bad_naming", $"\"{pattern}\" has no series placeholder.", "Use a pattern such as VSR-{seq}.", draft.Doctype);
                }

                draft.NamingSeries = naming.Naming;
                return null;
            }

            case SetPermissionsTemplateOp permissions:
            {
                if (PermissionsTemplates.Find(permissions.Template) is null)
                {
                    return Refuse("nocode_model.bad_target", $"\"{permissions.Template}\" is not a permissions template.", "Pick one of the listed templates.", draft.Doctype);
                }

                draft.PermissionsTemplate = permissions.Template;
                return null;
            }

            case AddSectionOp section:
            {
                var sections = draft.SectionsOf(section.ChildDoctype);
                if (sections.Any(s => s.Id == section.Id))
                {
                    return Refuse("nocode_model.duplicate_section", $"The section \"{section.Id}\" already exists.", "Pick another title.", draft.Doctype);
                }

                if (section.TabId is { } tab && !draft.Tabs.Any(t => t.Id == tab))
                {
                    return Refuse("nocode_model.unknown_tab", $"\"{tab}\" is not a tab of this form.", "Add the tab first.", draft.Doctype);
                }

                if (section.Columns is { } columns && columns is < 1 or > 4)
                {
                    return Refuse("nocode_model.bad_layout", "A section has one to four columns.", "Pick a column count between one and four.", draft.Doctype);
                }

                sections.Add(new FakeSection { Id = section.Id, Title = section.Title, Columns = section.Columns, TabId = section.TabId });
                return null;
            }

            case AddTabOp tab:
            {
                if (draft.Tabs.Any(t => t.Id == tab.Id))
                {
                    return Refuse("nocode_model.duplicate_tab", $"The tab \"{tab.Id}\" already exists.", "Pick another title.", draft.Doctype);
                }

                draft.Tabs.Add(new FakeTab { Id = tab.Id, Title = tab.Title });
                return null;
            }

            case SetSectionColumnsOp columns:
            {
                var section = draft.Section(columns.SectionId, columns.ChildDoctype);
                if (section is null)
                {
                    return Refuse("nocode_model.unknown_section", $"\"{columns.SectionId}\" is not a section of this form.", "Pick a section that exists.", draft.Doctype);
                }

                if (columns.Columns is < 1 or > 4)
                {
                    return Refuse("nocode_model.bad_layout", "A section has one to four columns.", "Pick a column count between one and four.", draft.Doctype);
                }

                section.Columns = columns.Columns;
                return null;
            }

            case MoveInLayoutOp move:
            {
                var fields = FieldsOfMutable(draft, move.ChildDoctype);
                if (fields?.FirstOrDefault(f => f.Name == move.Field) is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{move.Field}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, move.Field);
                }

                var section = draft.Section(move.SectionId, move.ChildDoctype);
                if (section is null)
                {
                    return Refuse("nocode_model.unknown_section", $"\"{move.SectionId}\" is not a section of this form.", "Drop the field on a section that exists.", draft.Doctype, move.Field);
                }

                if (move.After is { } anchor && !section.Items.Contains(anchor, StringComparer.Ordinal))
                {
                    return Refuse("nocode_model.bad_layout", $"\"{anchor}\" is not in the section \"{section.Id}\".", "Drop the field on a field of that section.", draft.Doctype, move.Field);
                }

                foreach (var other in draft.SectionsOf(move.ChildDoctype))
                {
                    other.Items.RemoveAll(name => name == move.Field);
                }

                var index = move.After is null ? -1 : section.Items.IndexOf(move.After);
                section.Items.Insert(index < 0 ? section.Items.Count : index + 1, move.Field);
                return null;
            }

            case SetDependsOnOp depends:
            {
                var field = draft.Field(depends.Name, depends.ChildDoctype);
                if (field is null)
                {
                    return Refuse("nocode_model.unknown_field", $"\"{depends.Name}\" is not a field of this draft.", "Pick a field that exists.", draft.Doctype, depends.Name);
                }

                if (depends.Condition is null)
                {
                    field.DependsOnText = null;
                    return null;
                }

                var text = Compile(depends.Condition, draft, depends.ChildDoctype, out var error);
                if (text is null)
                {
                    return Refuse("nocode_model.bad_condition", error ?? "The condition is not valid.", "Fix the rows in the condition builder.", draft.Doctype, depends.Name);
                }

                field.DependsOnText = text;
                return null;
            }

            default:
                return Refuse("nocode_model.unsupported_op", $"This server has no \"{op.Op}\" op.", "Nothing was changed.", draft.Doctype);
        }
    }

    private ValidationIssue Refuse(string code, string message, string suggestion, string sourceCode, string? field = null) =>
        new(code, sourceCode, message, suggestion, IssueSeverity.Error, Current.Doctype, field);

    private static List<FakeField>? FieldsOfMutable(FakeDraft draft, string? child) =>
        child is null ? draft.Fields : draft.Child(child)?.Fields;

    private static void InsertField(List<FakeField> fields, FakeField field, string? after)
    {
        var index = after is null ? -1 : fields.FindIndex(f => string.Equals(f.Name, after, StringComparison.Ordinal));
        if (index < 0)
        {
            fields.Add(field);
        }
        else
        {
            fields.Insert(index + 1, field);
        }
    }

    /// <summary>
    /// Compiles the builder's rows to expression text, exactly as <c>spot-nocode-model</c> prints
    /// it: <c>doc.status == "Open" or doc.status == "In Progress"</c>. A null text means a refusal.
    /// </summary>
    private static string? Compile(ConditionExpression condition, FakeDraft draft, string? child, out string? error)
    {
        error = null;
        if (condition.Rows.Count == 0)
        {
            error = "The condition has no rows.";
            return null;
        }

        var parts = new List<string>(condition.Rows.Count);
        foreach (var row in condition.Rows)
        {
            if (draft.Field(row.Field, child) is null)
            {
                error = $"\"{row.Field}\" is not a field of this DocType.";
                return null;
            }

            var expected = ConditionSyntax.ValueCount(row.Operator);
            if (expected is { } count && count != row.Values.Count)
            {
                error = $"\"{ConditionSyntax.Label(row.Operator)}\" needs {count} value(s).";
                return null;
            }

            if (row.Operator == ConditionOperator.In && row.Values.Count == 0)
            {
                error = "\"is one of\" needs at least one value.";
                return null;
            }

            parts.Add(Compare(row));
        }

        return string.Join(condition.Join == ConditionJoin.Or ? " or " : " and ", parts);
    }

    private static string Compare(ConditionRow row)
    {
        var path = $"doc.{row.Field}";
        return row.Operator switch
        {
            ConditionOperator.IsSet => $"{path} != null",
            ConditionOperator.IsNotSet => $"{path} == null",
            ConditionOperator.Equals => $"{path} == {Literal(row.Values[0])}",
            ConditionOperator.NotEquals => $"{path} != {Literal(row.Values[0])}",
            ConditionOperator.GreaterThan => $"{path} > {Literal(row.Values[0])}",
            ConditionOperator.GreaterOrEqual => $"{path} >= {Literal(row.Values[0])}",
            ConditionOperator.LessThan => $"{path} < {Literal(row.Values[0])}",
            ConditionOperator.LessOrEqual => $"{path} <= {Literal(row.Values[0])}",
            ConditionOperator.In => $"{path} in [{string.Join(", ", row.Values.Select(Literal))}]",
            _ => $"{path} == {Literal(row.Values[0])}",
        };
    }

    private static string Literal(ConditionValue value) => value.Kind switch
    {
        "int" => long.Parse(value.Text, CultureInfo.InvariantCulture).ToString(CultureInfo.InvariantCulture),
        "bool" => value.Text,
        _ => $"\"{value.Text}\"",
    };

    private static SampleStats StatsOf(FakeField? field)
    {
        if (field is null)
        {
            return SampleStats.Empty;
        }

        return field.Type switch
        {
            "text" or "long_text" => new SampleStats(Rows: 40, NonNull: 37, MaxTextLength: 64, Fractional: false, Unparsable: 5),
            "int" => new SampleStats(Rows: 40, NonNull: 40),
            "decimal" => new SampleStats(Rows: 40, NonNull: 40, Fractional: true),
            _ => new SampleStats(Rows: 40, NonNull: 12),
        };
    }
}
