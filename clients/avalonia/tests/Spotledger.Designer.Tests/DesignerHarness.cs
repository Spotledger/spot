using Spotledger.Client.Designer;
using Xunit;

namespace Spotledger.Designer.Tests;

/// <summary>
/// Drives the designer the way a person does: palette drops open the field dialog, the dialog's
/// create command adds the field, the inspector's commands edit it, and the canvas commands place
/// it. Every step is a UI command; the tests assert the ops those commands produced.
/// </summary>
internal sealed class DesignerHarness
{
    private DesignerHarness(FakeDesignerServer server, DesignerViewModel designer)
    {
        Server = server;
        Designer = designer;
    }

    public FakeDesignerServer Server { get; }

    public DesignerViewModel Designer { get; }

    public DesignerSession Session => Designer.Session;

    public static DesignerHarness Create(bool canPublish = true, FakeDraft? draft = null)
    {
        var server = new FakeDesignerServer(draft ?? FakeDesignerServer.NewVehicleDraft()) { CanPublish = canPublish };
        var session = new DesignerSession(server, "draft-vsr");
        return new DesignerHarness(server, new DesignerViewModel(session));
    }

    /// <summary>The ops the UI issued, in order, in a canonical one-line form.</summary>
    public IReadOnlyList<string> IssuedOps => Session.Sent.Select(OpWire.Of).ToList();

    public async Task<DesignerHarness> OpenAsync()
    {
        var result = await Designer.OpenAsync();
        Assert.True(result.Ok, result.Message);
        return this;
    }

    /// <summary>A field on the canvas, in the scope the canvas is showing unless one is named.</summary>
    public CanvasField Field(string name, string? child = null)
    {
        var scope = child ?? Designer.Canvas.ScopeChildDoctype;
        return Designer.Canvas.CurrentSections.SelectMany(s => s.Fields)
            .Concat(Designer.Canvas.CurrentUnplaced)
            .FirstOrDefault(f => string.Equals(f.Name, name, StringComparison.Ordinal)
                && string.Equals(f.ChildDoctype, scope, StringComparison.Ordinal))
            ?? throw new InvalidOperationException($"`{name}` is not on the canvas in scope `{scope ?? "<root>"}`.");
    }

    /// <summary>A palette drop: open the field dialog, name the field, add it.</summary>
    public async Task<DesignerResult> AddFieldAsync(string label, string type, string? sectionId = null, string? after = null, string? name = null)
    {
        await Designer.DropAsync(type, sectionId ?? string.Empty, after);
        Assert.True(Designer.FieldDialog.IsOpen, "the field dialog opened");
        Designer.FieldDialog.Label = label;
        if (name is not null)
        {
            Designer.FieldDialog.Name = name;
        }

        await Designer.FieldDialog.CreateAsync();
        return Session.LastResult;
    }

    /// <summary>The link picker's route: choose a DocType and add the link.</summary>
    public async Task<DesignerResult> AddLinkAsync(string label, string target)
    {
        await Designer.DropAsync("link", string.Empty);
        Assert.True(Designer.LinkPicker.IsOpen, "the link picker opened");
        Designer.LinkPicker.FieldLabel = label;
        await Designer.LinkPicker.SearchAsync();
        var choice = Designer.LinkPicker.Results.FirstOrDefault(c => string.Equals(c.Name, target, StringComparison.Ordinal));
        Assert.NotNull(choice);
        await Designer.LinkPicker.ChooseAsync(choice);
        return Session.LastResult;
    }

    /// <summary>The child-table dialog's route.</summary>
    public async Task<DesignerResult> AddChildTableAsync(string label, string childDoctype, string childLabel)
    {
        await Designer.DropAsync("table", string.Empty);
        Assert.True(Designer.ChildTableDialog.IsOpen, "the child-table dialog opened");
        Designer.ChildTableDialog.Title = label;
        Designer.ChildTableDialog.ChildDoctype = childDoctype;
        Designer.ChildTableDialog.ChildLabel = childLabel;
        await Designer.ChildTableDialog.CreateAsync();
        return Session.LastResult;
    }

    public async Task<DesignerResult> AddSectionAsync(string title, string id, int? columns = null)
    {
        var result = await Designer.Canvas.AddSectionAsync(title, null, columns, id);
        Assert.True(result.Ok, result.Message);
        return result;
    }

    public async Task<DesignerResult> PlaceAsync(string field, string sectionId, string? after = null, string? child = null)
    {
        var result = await Designer.Canvas.PlaceAsync(Field(field, child), sectionId, after);
        Assert.True(result.Ok, result.Message);
        return result;
    }

    public async Task SetOptionsAsync(string field, params string[] options)
    {
        Designer.SelectField(field);
        Designer.Inspector.Options.Clear();
        foreach (var option in options)
        {
            Designer.Inspector.Options.Add(new OptionRow(option));
        }

        await RunAsync(Designer.Inspector.ApplyOptionsCommand);
    }

    public async Task SetRequiredAsync(string field)
    {
        Designer.SelectField(field);
        Designer.Inspector.Required = true;
        await RunAsync(Designer.Inspector.ToggleRequiredCommand);
    }

    public async Task SetConditionAsync(string field, ConditionJoin join, params (string Field, string Value)[] rows)
    {
        Designer.SelectField(field);
        Designer.Inspector.Condition.Clear();
        foreach (var (rowField, value) in rows)
        {
            Designer.Inspector.Condition.Add(rowField, ConditionOperator.Equals, value);
        }

        Designer.Inspector.UseOr = join == ConditionJoin.Or;
        await RunAsync(Designer.Inspector.ApplyConditionCommand);
    }

    public void OpenChild(string doctype) => Designer.Canvas.OpenChildGrid(doctype);

    public void CloseChild() => Designer.Canvas.CloseChildGrid();

    /// <summary>Runs a command and waits for the work it started.</summary>
    public static async Task RunAsync(AsyncRelayCommand command)
    {
        command.Execute(null);
        await command.ExecutionTask;
    }

    /// <summary>
    /// The canonical wire text of an op: one line, `-` for an absent part, so the tests can compare
    /// the UI's op stream with the server-side golden sequence line for line.
    /// </summary>
    public static class OpWire
    {
        public static string Of(DesignerOp op)
        {
            ArgumentNullException.ThrowIfNull(op);
            return op switch
            {
                AddFieldOp add => Line("add_field", add.Name, add.Label, add.Type, add.InsertAfter, add.Child),
                RenameFieldOp rename => Line("rename_field", rename.Name, rename.To, rename.Child),
                RemoveFieldOp remove => Line("remove_field", remove.Name, remove.Confirmation, remove.Child),
                ChangeTypeOp change => Line("change_type", change.Name, change.Type, change.Child),
                SetOptionsOp options => Line("set_options", options.Name, string.Join(",", options.Options), options.Child),
                SetRequiredOp required => Line("set_required", required.Name, required.Required ? "true" : "false", required.Child),
                SetDefaultOp defaults => Line("set_default", defaults.Name, defaults.Default, defaults.Child),
                ReorderFieldOp reorder => Line("reorder_field", reorder.Name, reorder.After, reorder.Child),
                AddChildTableOp table => Line("add_child_table", table.Name, table.Label, table.ChildDoctypeName, table.ChildTitle, table.InsertAfter),
                AddLinkOp link => Line("add_link", link.Name, link.Label, link.Target, link.InsertAfter),
                SetNamingSeriesOp naming => Line("set_naming_series", naming.Naming),
                SetPermissionsTemplateOp permissions => Line("set_permissions_template", permissions.Template),
                AddSectionOp section => Line("add_form_section", section.Id, section.Title, section.Columns, section.TabId, section.Child),
                AddTabOp tab => Line("add_form_tab", tab.Id, tab.Title, tab.Child),
                SetSectionColumnsOp columns => Line("set_section_columns", columns.SectionId, columns.Columns, columns.Child),
                MoveInLayoutOp move => Line("move_field_in_layout", move.Field, move.SectionId, move.After, move.Child),
                SetDependsOnOp depends => Line("set_depends_on", depends.Name, depends.Condition is null ? null : Condition(depends.Condition), depends.Child),
                _ => op.Op,
            };
        }

        public static string Condition(ConditionExpression condition) =>
            string.Join(
                condition.Join == ConditionJoin.Or ? " or " : " and ",
                condition.Rows.Select(row => $"{row.Field} {ConditionSyntax.Name(row.Operator)} {string.Join(",", row.Values.Select(v => v.Text))}"));

        private static string Line(params object?[] parts) =>
            string.Join("|", parts.Select(part => part?.ToString() ?? "-"));
    }
}
