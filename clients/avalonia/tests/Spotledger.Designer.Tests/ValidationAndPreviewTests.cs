using Spotledger.Client.Designer;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Designer.Tests;

/// <summary>
/// The validation panel and the preview: server messages in plain language, a click that selects
/// the element an issue is about, and a preview built from the server's metadata that renders the
/// real form controls and persists nothing.
/// </summary>
public sealed class ValidationAndPreviewTests
{
    [Fact]
    public async Task ServerIssueSelectsTheElementItNames()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddFieldAsync("Title", "text");
        await harness.AddFieldAsync("Priority", "select");
        await harness.AddSectionAsync("Request", "request", 2);
        await harness.PlaceAsync("title", "request");
        await harness.PlaceAsync("priority", "request");

        var panel = harness.Designer.Validation;
        Assert.Equal(1, panel.ErrorCount);
        Assert.False(panel.IsOk);
        Assert.Equal("1 to fix", panel.Summary);

        var issue = Assert.Single(panel.Issues);
        Assert.Equal("nocode_model.bad_options", issue.Code);
        Assert.Equal(IssueSeverity.Error, issue.Severity);
        Assert.Equal("priority", issue.Field);
        Assert.Contains("Priority", issue.Message, StringComparison.Ordinal);
        Assert.Contains("Add at least one value", issue.Suggestion, StringComparison.Ordinal);
        Assert.Equal("Must fix", issue.SeverityText);

        // Clicking the issue selects the field on the canvas and in the inspector.
        harness.Designer.Canvas.SelectedField = null;
        panel.SelectIssueCommand.Execute(issue);

        Assert.Equal("priority", harness.Designer.Canvas.SelectedField!.Name);
        Assert.Equal("priority", harness.Designer.Inspector.Field!.Name);
        Assert.Equal("vehicle_service_request.priority", harness.Session.SelectedElement);
        Assert.Contains(issue.Message, harness.Designer.StatusText, StringComparison.Ordinal);

        // Fixing the design clears the panel: the server validates, the panel only shows it.
        await harness.SetOptionsAsync("priority", "Low", "High");
        Assert.Equal(0, panel.ErrorCount);
        Assert.Equal("No issues", panel.Summary);
    }

    [Fact]
    public async Task EmptySectionAndUnplacedFieldAreWarnings()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddFieldAsync("Title", "text");
        await harness.AddSectionAsync("Request", "request", 2);
        await harness.AddSectionAsync("Notes", "notes", 1);

        var panel = harness.Designer.Validation;
        // One field off the form and two empty sections: three things to look at, nothing to fix.
        Assert.Equal(0, panel.ErrorCount);
        Assert.Equal(3, panel.WarningCount);
        Assert.Equal("3 to check", panel.Summary);
        Assert.True(panel.IsOk);
    }

    [Fact]
    public async Task PreviewRendersTheRealFormAtBothWidthsAndAsAList()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddFieldAsync("Customer Name", "text");
        await harness.AddFieldAsync("Priority", "select");
        await harness.SetOptionsAsync("priority", "Low", "Medium");
        await harness.AddFieldAsync("Request Date", "date");
        await harness.AddSectionAsync("Request", "request", 2);
        await harness.PlaceAsync("customer_name", "request");
        await harness.PlaceAsync("priority", "request");
        await harness.PlaceAsync("request_date", "request");

        var saves = harness.Server.Saves.Count;
        var version = harness.Server.Current.Version;

        var preview = harness.Designer.Preview;
        await preview.ShowAsync(PreviewLayout.Desktop);
        Assert.True(preview.IsDesktop);
        Assert.Equal(3, preview.Sections.Count);
        Assert.Equal(2, preview.Preview!.FormLayout.Section("request")!.Columns);
        Assert.Equal(3, preview.ListColumns.Count);

        // The board holds the same metadata the real form gets: the palette's kind per field, and
        // the sample values the server supplied, in a draft that is never saved.
        var board = preview.Board!;
        Assert.NotNull(board);
        Assert.Equal(FieldKind.Select, board.Model.Field("priority")!.Kind);
        Assert.Equal(FieldKind.Date, board.Model.Field("request_date")!.Kind);
        var sample = Assert.IsType<TextFieldValue>(board.State("customer_name")!.Value);
        Assert.Equal("Customer Name sample", sample.Value);
        Assert.Equal(3, board.Draft.Fields.Count);

        await preview.ShowAsync(PreviewLayout.Narrow);
        Assert.True(preview.IsNarrow);
        Assert.All(preview.Preview!.FormLayout.SectionList, section => Assert.Equal(1, section.Columns));

        await preview.ShowAsync(PreviewLayout.List);
        Assert.True(preview.IsList);
        Assert.NotNull(preview.Preview!.List);
        Assert.Equal(3, preview.ListColumns.Count);

        // Previewing never saves, never bumps the draft and never publishes.
        Assert.Equal(saves, harness.Server.Saves.Count);
        Assert.Equal(version, harness.Server.Current.Version);
        Assert.Equal(3, harness.Server.PreviewCount);
        Assert.Equal(0, harness.Server.PublishCount);
    }

    [Fact]
    public async Task ChildTablePreviewCarriesItsOwnGrid()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddFieldAsync("Title", "text");
        await harness.AddChildTableAsync("Service Items", "vehicle_service_item", "Vehicle Service Item");
        harness.OpenChild("vehicle_service_item");
        await harness.AddFieldAsync("Item", "text", name: "item_name");
        await harness.AddFieldAsync("Hours", "int");
        await harness.AddSectionAsync("Items", "items_grid", 2);
        await harness.PlaceAsync("item_name", "items_grid");
        await harness.PlaceAsync("hours", "items_grid");
        harness.CloseChild();

        await harness.Designer.Preview.ShowAsync(PreviewLayout.Desktop);

        var child = Assert.Single(harness.Designer.Preview.Preview!.ChildList);
        Assert.Equal("vehicle_service_item", child.Doctype);
        Assert.Equal(2, child.Fields!.Count);
        Assert.Equal(2, child.Grid!.Section("items_grid")!.Columns);

        // The table field knows its row DocType, so the preview can render the grid it points at.
        var tableField = harness.Designer.Preview.Board!.Model.Field("service_items")!;
        Assert.Equal(FieldKind.Table, tableField.Kind);
        Assert.Equal("vehicle_service_item", tableField.ChildDoctype);
        Assert.NotNull(harness.Designer.Preview.Board.Model.ChildTable("service_items"));
    }

    [Fact]
    public void FieldNamesFollowTheServerNamingRules()
    {
        Assert.Equal("request_date", DesignerNaming.SafeIdentifier("Request Date"));
        Assert.Equal("f2_things", DesignerNaming.SafeIdentifier("2 things"));
        Assert.True(DesignerNaming.IsStorageName("custom_notes"));
        Assert.False(DesignerNaming.IsStorageName("Notes"));
        Assert.False(DesignerNaming.IsStorageName("9lives"));
        Assert.False(DesignerNaming.IsStorageName(string.Empty));

        // A field added to an existing DocType carries the custom_ prefix; a new DocType's does not.
        Assert.Equal("priority", DesignerNaming.FieldName("Priority", existingDocType: false));
        Assert.Equal("custom_priority", DesignerNaming.FieldName("Priority", existingDocType: true));
        Assert.Equal("custom_priority", DesignerNaming.FieldName("Custom Priority", existingDocType: true));

        var taken = new[] { "priority" };
        var twoTaken = new[] { "priority", "priority_2" };
        Assert.Equal("priority_2", DesignerNaming.Unique(taken, "priority"));
        Assert.Equal("priority_3", DesignerNaming.Unique(twoTaken, "priority"));
        Assert.Equal("priority", DesignerNaming.Unique(Array.Empty<string>(), "priority"));
    }

    [Fact]
    public void EveryPaletteEntryMapsToAControlKind()
    {
        Assert.NotEmpty(FieldTypeCatalog.All);
        foreach (var info in FieldTypeCatalog.All)
        {
            Assert.False(string.IsNullOrWhiteSpace(info.Title));
            Assert.False(string.IsNullOrWhiteSpace(info.Description));
            Assert.NotEqual(FieldKind.Unknown, info.Kind);
            Assert.Equal(info.Kind, FieldTypeCatalog.KindOf(info.Type));
        }

        Assert.True(FieldTypeCatalog.Find("link")!.NeedsTarget);
        Assert.True(FieldTypeCatalog.Find("table")!.IsChildTable);
        Assert.True(FieldTypeCatalog.Find("select")!.NeedsOptions);

        // A type the palette does not know degrades to Unknown: the designer never invents one.
        Assert.Equal(FieldKind.Unknown, FieldTypeCatalog.KindOf("hologram"));
        Assert.Null(FieldTypeCatalog.Find("hologram"));
    }
}
