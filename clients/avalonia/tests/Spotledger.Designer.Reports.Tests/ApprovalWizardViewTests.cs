using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Spotledger.Designer.Reports;
using Spotledger.Designer.Reports.Views;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

/// <summary>The approval wizard view, shown in a headless window and driven through its controls.</summary>
public sealed class ApprovalWizardViewTests
{
    private static void Fill(ApprovalWizardViewModel wizard)
    {
        wizard.DocType = "Leave Application";
        wizard.ApproverName = "manager";
        wizard.EscalationName = "director";
        wizard.ApproveField = "status";
        wizard.ApproveValue = "Approved";
        wizard.SubmitOnApprove = true;
        wizard.RejectField = "status";
        wizard.RejectValue = "Rejected";
    }

    private static string VisibleStep(Shown<ApprovalWizardView, ApprovalWizardViewModel> shown)
    {
        var visible = new[] { "DocumentStep", "ApproverStep", "LimitStep", "OutcomesStep", "ReviewStep" }
            .Where(name => shown.Find<StackPanel>(name).IsVisible)
            .ToList();
        return Assert.Single(visible);
    }

    [AvaloniaFact]
    public void One_step_shows_at_a_time_and_a_step_that_is_not_filled_in_says_what_is_missing()
    {
        var shown = Views.Wizard(new FakeBuilderServer());
        Assert.Equal("DocumentStep", VisibleStep(shown));

        Clicks.On(shown.Find<Button>("NextButton"));
        Assert.True(Pumping.WaitAsync(() => !string.IsNullOrEmpty(shown.Find<TextBlock>("ProblemText").Text)).GetAwaiter().GetResult());
        Assert.Equal("DocumentStep", VisibleStep(shown));
    }

    [AvaloniaFact]
    public async Task A_person_completes_the_wizard_through_the_views_buttons_and_publishes_the_servers_template()
    {
        var server = new FakeBuilderServer();
        var shown = Views.Wizard(server);

        // Choose the document type by searching for it.
        shown.Find<TextBox>("DocTypeBox").Text = "Sales";
        Assert.True(await Pumping.WaitAsync(() => shown.Model.DocTypeChoices.Count == 1));
        shown.Find<ListBox>("DocTypeList").SelectedIndex = 0;
        Assert.Equal("Sales Invoice", shown.Model.DocType);

        // The pickers write to the view-model.
        shown.Find<ComboBox>("ApproverKindBox").SelectedItem = ApproverKind.ManagerChain;
        shown.Find<ComboBox>("UnitBox").SelectedItem = TimeUnit.Hours;
        shown.Find<ComboBox>("EscalationKindBox").SelectedItem = ApproverKind.User;
        Assert.Equal(ApproverKind.ManagerChain, shown.Model.ApproverKind);
        Assert.Equal(TimeUnit.Hours, shown.Model.Unit);
        Assert.Equal(ApproverKind.User, shown.Model.EscalationKind);

        Fill(shown.Model);
        shown.Model.DocType = "Leave Application";
        var expected = new[] { "ApproverStep", "LimitStep", "OutcomesStep", "ReviewStep" };
        foreach (var step in expected)
        {
            Clicks.On(shown.Find<Button>("NextButton"));
            Assert.True(await Pumping.WaitAsync(() => VisibleStep(shown) == step));
        }

        // The review step draws the read-only flow as five boxes.
        Assert.Equal(5, shown.Find<StackPanel>("ReviewStep").GetVisualDescendantsOfText().Count(t => t.Length > 0 && !t.Equals("v", StringComparison.Ordinal) && !t.Equals("The flow", StringComparison.Ordinal)));
        Assert.Contains("Approved: set status to Approved, then submit the document", shown.Texts());

        Clicks.On(shown.Find<Button>("PublishButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Find<TextBlock>("ResultText").Text == "Saved as a draft process."));
        Assert.Single(server.PublishedApprovals);
        Clicks.On(shown.Find<Button>("BackButton"));
        Assert.Equal("OutcomesStep", VisibleStep(shown));
    }

    [AvaloniaFact]
    public async Task A_dropped_connection_on_the_review_step_is_shown_on_the_wizard()
    {
        var server = new FakeBuilderServer();
        var shown = Views.Wizard(server);
        Fill(shown.Model);
        for (var i = 0; i < 3; i++)
        {
            Clicks.On(shown.Find<Button>("NextButton"));
            await Pumping.WaitAsync(() => !shown.Model.NextCommand.IsRunning);
        }

        server.Offline = true;
        Clicks.On(shown.Find<Button>("NextButton"));
        Assert.True(await Clicks.StatusContainsAsync(shown.Find<TextBlock>("ProblemText"), "That did not work"));
        Assert.Equal("OutcomesStep", VisibleStep(shown));
    }

    [AvaloniaFact]
    public async Task The_wizard_renders_a_real_picture_on_the_review_step()
    {
        var shown = Views.Wizard(new FakeBuilderServer());
        Fill(shown.Model);
        for (var i = 0; i < 4; i++)
        {
            Clicks.On(shown.Find<Button>("NextButton"));
            await Pumping.WaitAsync(() => !shown.Model.NextCommand.IsRunning);
        }

        Assert.Equal("ReviewStep", VisibleStep(shown));
        Assert.True(Views.DistinctColours(shown.Window, "approval-wizard") > 40, "the wizard frame looks blank");
    }
}
