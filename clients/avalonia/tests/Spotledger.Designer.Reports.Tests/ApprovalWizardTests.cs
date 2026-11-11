using Spotledger.Client.Designer;
using Spotledger.Designer.Reports;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

public sealed class ApprovalWizardTests
{
    private const string Golden =
        """{"name":"approve_leave_application","title":"Leave Application approval","app":"hr","doctype":"Leave Application","approvers":{"kind":"role","role":"manager"},"sla_seconds":172800,"escalate_to":{"kind":"role","role":"director"},"on_approve":[{"kind":"set_field","field":"status","value":"Approved"},{"kind":"submit"}],"on_reject":[{"kind":"set_field","field":"status","value":"Rejected"}],"start_on":"created"}""";

    private static ApprovalWizardViewModel Filled(FakeBuilderServer server)
    {
        return new ApprovalWizardViewModel(server, "hr")
        {
            DocType = "Leave Application",
            ApproverKind = ApproverKind.Role,
            ApproverName = "manager",
            Limit = 2,
            Unit = TimeUnit.Days,
            EscalationKind = ApproverKind.Role,
            EscalationName = "director",
            ApproveField = "status",
            ApproveValue = "Approved",
            SubmitOnApprove = true,
            RejectField = "status",
            RejectValue = "Rejected",
        };
    }

    [Fact]
    public async Task The_wizard_output_is_exactly_the_servers_template_input()
    {
        var wizard = Filled(new FakeBuilderServer());

        Assert.Equal(Golden, BuilderJson.ToJson(wizard.BuildTemplate()).ToJsonString());
        await Task.CompletedTask;
    }

    [Fact]
    public async Task A_person_completes_the_wizard_with_only_the_next_and_publish_commands()
    {
        var server = new FakeBuilderServer();
        var wizard = Filled(server);

        for (var i = 0; i < 4; i++)
        {
            await Support.RunAsync(wizard.NextCommand);
        }

        Assert.Equal(WizardStep.Review, wizard.Step);
        Assert.Equal(5, wizard.Flow.Count);
        Assert.Equal("Anyone with the role manager approves, within 2 days", wizard.Flow[1].Text);
        Assert.Equal("Approved: set status to Approved, then submit the document", wizard.Flow[3].Text);

        await Support.RunAsync(wizard.PublishCommand);

        Assert.Equal("Saved as a draft process.", wizard.Result);
        Assert.Equal([Golden], server.PublishedApprovals);
    }

    [Fact]
    public async Task Each_step_says_what_is_missing_before_it_lets_the_person_move_on()
    {
        var wizard = new ApprovalWizardViewModel(new FakeBuilderServer(), "hr");

        await Support.RunAsync(wizard.NextCommand);
        Assert.Equal(WizardStep.Document, wizard.Step);
        Assert.Equal("Choose the document that needs approval.", wizard.Problem);

        wizard.DocType = "Purchase Order";
        await Support.RunAsync(wizard.NextCommand);
        Assert.Equal(WizardStep.Approver, wizard.Step);
        Assert.Equal(string.Empty, wizard.Problem);

        await Support.RunAsync(wizard.NextCommand);
        Assert.StartsWith("Say who approves", wizard.Problem, StringComparison.Ordinal);

        wizard.ApproverName = "buyer";
        await Support.RunAsync(wizard.NextCommand);
        await Support.RunAsync(wizard.NextCommand);
        Assert.StartsWith("Say who is asked", wizard.Problem, StringComparison.Ordinal);

        wizard.BackCommand.Execute(null);
        Assert.Equal(WizardStep.Approver, wizard.Step);
    }

    [Fact]
    public async Task The_server_rejects_an_approver_it_cannot_resolve_and_publish_stays_off()
    {
        var wizard = Filled(new FakeBuilderServer());
        wizard.ApproverName = "Line Manager!";

        for (var i = 0; i < 4; i++)
        {
            await Support.RunAsync(wizard.NextCommand);
        }

        Assert.Single(wizard.Issues);
        Assert.Contains("not a name the site can resolve", wizard.Issues[0].Message, StringComparison.Ordinal);
        Assert.False(wizard.PublishCommand.CanExecute(null));
    }

    [Fact]
    public void Hours_and_the_manager_chain_and_manual_start_map_onto_the_template()
    {
        var wizard = new ApprovalWizardViewModel(new FakeBuilderServer(), "sales")
        {
            DocType = "Quotation",
            ApproverKind = ApproverKind.ManagerChain,
            ApproverName = "owner",
            Limit = 6,
            Unit = TimeUnit.Hours,
            EscalationKind = ApproverKind.User,
            EscalationName = "cfo",
            StartOn = StartOn.Manual,
        };

        var template = wizard.BuildTemplate();

        Assert.Equal(21_600, template.SlaSeconds);
        Assert.Empty(template.OnApprove);
        var json = BuilderJson.ToJson(template);
        Assert.Equal("manager_chain", (string?)json["approvers"]!["kind"]);
        Assert.Equal("user", (string?)json["escalate_to"]!["kind"]);
        Assert.Equal("manual", (string?)json["start_on"]);
        Assert.Equal("approve_quotation", template.Name);
    }

    [Fact]
    public void The_builders_register_into_the_designer_shell_in_a_fixed_order()
    {
        Assert.Equal(["report-builder", "dashboard-builder", "approval-wizard"], BuilderTools.All.Select(t => t.Id));

        // The host's one call fills the registry the shell reads, and each tool adapts the shell's
        // neutral context to this project's server surface.
        var registry = DesignerTools.Default();
        Assert.Equal(["report-builder", "dashboard-builder", "approval-wizard"], registry.Tools.Select(t => t.Id));
        var context = new DesignerToolContext(new FakeBuilderServer(), "rpt-1");
        Assert.IsType<ReportBuilderViewModel>(registry.Find("report-builder")!.Create(context));
        Assert.IsType<DashboardBuilderViewModel>(registry.Find("dashboard-builder")!.Create(context));
        Assert.IsType<ApprovalWizardViewModel>(registry.Find("approval-wizard")!.Create(context));
    }

    [Fact]
    public void A_tool_refuses_a_connection_it_cannot_use_with_a_sentence()
    {
        var context = new DesignerToolContext(new object(), "rpt-1");

        var refusal = Assert.Throws<InvalidOperationException>(() => BuilderTools.Report.Factory(context));
        Assert.Contains("IBuilderServer", refusal.Message, StringComparison.Ordinal);
    }
}
