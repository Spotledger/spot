using Spotledger.Client.Designer;
using Xunit;

namespace Spotledger.Designer.Tests;

/// <summary>
/// The server-side golden sequence, rebuilt through the designer's own commands. The expected op
/// lines are transcribed one for one from <c>crates/framework/spot-nocode-model/tests/support/mod.rs</c>
/// (<c>build_vehicle_service_request</c>), so a change in the UI's command layer that changes the
/// ops it sends breaks this test rather than quietly producing a different design.
/// </summary>
public sealed class GoldenDesignTests
{
    private static readonly string[] ExpectedGoldenOps =
    {
        "add_field|request_date|Request Date|date|-|-",
        "add_field|customer_name|Customer Name|text|-|-",
        "add_field|priority|Priority|select|-|-",
        "set_options|priority|Low,Medium,High|-",
        "add_field|status|Status|select|-|-",
        "set_options|status|Open,In Progress,Closed|-",
        "add_link|vehicle|Vehicle|vehicle|-",
        "add_field|description|Description|long_text|-|-",
        "add_field|customer_phone|Customer Phone|text|-|-",
        "add_child_table|service_items|Service Items|vehicle_service_item|Vehicle Service Item|-",
        "add_field|item_name|Item|text|-|vehicle_service_item",
        "set_required|item_name|true|vehicle_service_item",
        "add_field|hours|Hours|int|-|vehicle_service_item",
        "add_field|notes|Notes|text|-|vehicle_service_item",
        "set_depends_on|customer_phone|status equals Open or status equals In Progress|-",
        "add_form_section|request|Request|2|-|-",
        "move_field_in_layout|request_date|request|-|-",
        "move_field_in_layout|customer_name|request|-|-",
        "move_field_in_layout|priority|request|-|-",
        "move_field_in_layout|status|request|-|-",
        "move_field_in_layout|vehicle|request|-|-",
        "move_field_in_layout|description|request|-|-",
        "move_field_in_layout|customer_phone|request|-|-",
        "add_form_section|items|Service Items|-|-|-",
        "move_field_in_layout|service_items|items|-|-",
        "add_form_section|items_grid|Items|-|-|vehicle_service_item",
        "move_field_in_layout|item_name|items_grid|-|vehicle_service_item",
        "move_field_in_layout|hours|items_grid|-|vehicle_service_item",
        "move_field_in_layout|notes|items_grid|-|vehicle_service_item",
    };

    private static readonly string[] RootFieldsInOrder =
    {
        "request_date", "customer_name", "priority", "status", "vehicle", "description", "customer_phone", "service_items",
    };

    private static readonly string[] RequestItemsInOrder =
    {
        "request_date", "customer_name", "priority", "status", "vehicle", "description", "customer_phone",
    };

    private static readonly string[] ChildFieldsInOrder = { "item_name", "hours", "notes" };

    [Fact]
    public async Task BuildsTheGoldenDesignWithTheUiCommands()
    {
        var harness = await BuildVehicleServiceRequestAsync();

        Assert.Equal(ExpectedGoldenOps, harness.IssuedOps);
        Assert.Equal(ExpectedGoldenOps.Length, harness.Session.Sent.Count);
    }

    [Fact]
    public async Task BuiltDesignHasTheGoldenShape()
    {
        var harness = await BuildVehicleServiceRequestAsync();
        var draft = harness.Server.Current;

        Assert.Equal("vehicle_service_request", draft.Doctype);
        Assert.Equal("Vehicle Service Request", draft.Label);
        Assert.Equal(RootFieldsInOrder, draft.Fields.Select(f => f.Name).ToArray());

        var priorities = new[] { "Low", "Medium", "High" };
        var statuses = new[] { "Open", "In Progress", "Closed" };
        Assert.Equal(priorities, draft.Field("priority", null)!.Options.ToArray());
        Assert.Equal(statuses, draft.Field("status", null)!.Options.ToArray());
        Assert.Equal("vehicle", draft.Field("vehicle", null)!.LinkTarget);
        Assert.Equal("vehicle_service_item", draft.Field("service_items", null)!.ChildDoctype);

        var child = draft.Child("vehicle_service_item");
        Assert.NotNull(child);
        Assert.Equal("Vehicle Service Item", child!.Label);
        Assert.Equal(ChildFieldsInOrder, child.Fields.Select(f => f.Name).ToArray());
        Assert.True(child.Field("item_name")!.Required);

        // The compiled condition is the server's own words for the two rows, joined with "or".
        Assert.Equal("doc.status == \"Open\" or doc.status == \"In Progress\"", draft.Field("customer_phone", null)!.DependsOnText);

        var request = draft.Section("request", null);
        Assert.NotNull(request);
        Assert.Equal(2, request!.Columns);
        Assert.Equal(RequestItemsInOrder, request.Items.ToArray());

        var tableSection = new[] { "service_items" };
        Assert.Equal(tableSection, draft.Section("items", null)!.Items.ToArray());
        Assert.Equal(ChildFieldsInOrder, draft.Section("items_grid", "vehicle_service_item")!.Items.ToArray());

        // What the server says about it: the golden design validates with no issues at all.
        Assert.NotNull(harness.Session.Validation);
        Assert.True(harness.Session.Validation!.IsOk);
        Assert.Empty(harness.Session.Validation.IssueList);
    }

    [Fact]
    public async Task GoldenDesignPreviewsAndPublishesAsANewDoctype()
    {
        var harness = await BuildVehicleServiceRequestAsync();

        await harness.Designer.Preview.ShowAsync(PreviewLayout.Desktop);
        var preview = harness.Designer.Preview.Preview!;
        Assert.True(preview.Valid);
        Assert.True(preview.Sandbox);
        Assert.Equal("vehicle_service_request", preview.Doctype);
        Assert.Equal("Vehicle Service Request", preview.Label);
        Assert.Contains(preview.FormLayout.SectionList, s => s.Id == "request");
        Assert.Contains(preview.FormLayout.SectionList, s => s.Id == "items");
        Assert.Single(preview.ChildList);
        Assert.Equal("vehicle_service_item", preview.ChildList[0].Doctype);
        Assert.Equal("Customer Phone", preview.Field("customer_phone")!.Label);

        await harness.Designer.Publish.OpenAsync();
        var plan = harness.Designer.Publish.Plan!;
        Assert.True(plan.Ready);
        Assert.True(plan.NewDoctype);
        Assert.Equal(PublishRisk.Additive, plan.Risk);
        Assert.Contains(plan.ChangeList, c => c.Kind == "doctype" && c.Summary.Contains("vehicle_service_request", StringComparison.Ordinal));

        harness.Designer.Publish.IsConfirmed = true;
        Assert.True(harness.Designer.Publish.CanConfirm);
        await harness.Designer.Publish.ConfirmAsync();

        Assert.Equal(1, harness.Server.PublishCount);
        Assert.Equal("published", harness.Server.Current.Status);
        Assert.NotNull(harness.Session.LastReceipt);
        Assert.Equal(1, harness.Session.LastReceipt!.RecordsWritten);
    }

    /// <summary>The golden build, through the commands the designer's views invoke.</summary>
    private static async Task<DesignerHarness> BuildVehicleServiceRequestAsync()
    {
        var harness = await DesignerHarness.Create().OpenAsync();

        // The fields first, then the layout: the server-side golden sequence's own order.
        await harness.AddFieldAsync("Request Date", "date");
        await harness.AddFieldAsync("Customer Name", "text");
        await harness.AddFieldAsync("Priority", "select");
        await harness.SetOptionsAsync("priority", "Low", "Medium", "High");
        await harness.AddFieldAsync("Status", "select");
        await harness.SetOptionsAsync("status", "Open", "In Progress", "Closed");
        await harness.AddLinkAsync("Vehicle", "vehicle");
        await harness.AddFieldAsync("Description", "long_text");
        await harness.AddFieldAsync("Customer Phone", "text");
        await harness.AddChildTableAsync("Service Items", "vehicle_service_item", "Vehicle Service Item");

        harness.OpenChild("vehicle_service_item");
        await harness.AddFieldAsync("Item", "text", name: "item_name");
        await harness.SetRequiredAsync("item_name");
        await harness.AddFieldAsync("Hours", "int");
        await harness.AddFieldAsync("Notes", "text");
        harness.CloseChild();

        await harness.SetConditionAsync(
            "customer_phone",
            ConditionJoin.Or,
            ("status", "Open"),
            ("status", "In Progress"));

        await harness.AddSectionAsync("Request", "request", 2);
        foreach (var name in RequestItemsInOrder)
        {
            await harness.PlaceAsync(name, "request");
        }

        await harness.AddSectionAsync("Service Items", "items");
        await harness.PlaceAsync("service_items", "items");

        harness.OpenChild("vehicle_service_item");
        await harness.AddSectionAsync("Items", "items_grid");
        foreach (var name in ChildFieldsInOrder)
        {
            await harness.PlaceAsync(name, "items_grid");
        }

        harness.CloseChild();
        return harness;
    }
}
