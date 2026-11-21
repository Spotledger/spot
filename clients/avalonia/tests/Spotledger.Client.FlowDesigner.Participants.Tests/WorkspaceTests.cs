using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media.Imaging;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The workspace puts the canvas, the contract editor's panels, the simulator and the effect lists
/// together. Every list is the session's words, so these tests read what a person would read.
public sealed class WorkspaceTests
{

    private static ParticipantWorkspace Workspace(
        out ParticipantSession session,
        out FakeParticipantServer server,
        out FakeSimulatorServer simulator)
    {
        server = new FakeParticipantServer();
        simulator = new FakeSimulatorServer();
        FlowDocument document = ParticipantFixtures.BuyerHalf();
        session = new ParticipantSession(
            server,
            simulator,
            document,
            new FlowEditor(document),
            new ContractEditor(ParticipantFixtures.Contract(), ParticipantFixtures.SecretCostCode()),
            ParticipantFixtures.Buyer,
            ParticipantFixtures.ServerKinds());
        ParticipantWorkspace workspace = new(session);
        TestHost.Show(workspace, 1200, 800);
        workspace.Canvas.ApplyAutoLayout();
        workspace.Canvas.FitToLanes();
        TestHost.Pump();
        return workspace;
    }

    [AvaloniaFact]
    public void ThePanelsReadTheContractInThePersonsWords()
    {
        ParticipantWorkspace workspace = Workspace(out _, out _, out _);

        Assert.Equal(
            ["You: buyer (organisation)", "approver (organisation)"],
            workspace.RoleList.ItemsSource!.Cast<string>());
        List<string> messages = [.. workspace.MessageList.ItemsSource!.Cast<string>()];
        Assert.Equal("approval_request: buyer to approver on procurement.approval, schema 1", messages[0]);
        Assert.Contains("  amount_minor (integer, required)", messages);
        Assert.Contains("  cost_code (string, optional, up to 20) - SECRET, never crosses", messages);
        Assert.Equal(
            [
                "at the start, buyer sends approval_request",
                "after approval_request, approver sends approval_decision",
                "after approval_request, approver sends documents_requested",
                "after approval_decision, buyer ends in settled",
                "after approval_request, buyer ends in expired, at most 1 times",
            ],
            workspace.OrderList.ItemsSource!.Cast<string>());
        Assert.Equal(["settled (completed)", "expired (failed)"], workspace.OutcomeList.ItemsSource!.Cast<string>());
        Assert.Equal(["buyer waits 86400s for approval_decision, then ends in expired"], workspace.TimeoutList.ItemsSource!.Cast<string>());
        Assert.Equal(["buyer: ed25519 key key-buyer-1 - Not checked yet"], workspace.SignatureList.ItemsSource!.Cast<string>());
        Assert.Equal(6, workspace.Diagram.Diagram.Nodes.Count);
    }

    [AvaloniaFact]
    public void AnEditedOrderRuleChangesTheTableAndTheDiagramTogether()
    {
        ParticipantWorkspace workspace = Workspace(out ParticipantSession session, out _, out _);

        Assert.True(session.Contract.RemoveTransition(2).Ok);
        TestHost.Pump();

        Assert.Equal(4, workspace.OrderList.ItemsSource!.Cast<string>().Count());
        Assert.Equal(4, workspace.Diagram.Diagram.Edges.Count);
        Assert.Contains(workspace.Diagram.Diagram.Unreachable, node => node.Label == "documents_requested");
    }

    [AvaloniaFact]
    public void TheContractFileExportsAndImportsBackThroughTheBox()
    {
        ParticipantWorkspace workspace = Workspace(out ParticipantSession session, out _, out _);
        workspace.Export();
        string file = workspace.ContractFileBox.Text!;
        Assert.Equal(ParticipantFixtures.Contract().ToText(), file);

        session.Contract.AddOutcome("withdrawn", ContractOutcomeKind.Cancelled);
        Assert.Equal(3, session.Contract.Current.Outcomes.Count);

        workspace.ContractFileBox.Text = file;
        Assert.True(workspace.Import().Ok);
        Assert.Equal(2, session.Contract.Current.Outcomes.Count);

        workspace.ContractFileBox.Text = "garbage";
        Assert.False(workspace.Import().Ok);
        Assert.NotNull(workspace.StatusLine.Text);
        workspace.ContractFileBox.Text = "  ";
        Assert.Equal("Paste a contract file first.", workspace.Import().Error);
    }

    [AvaloniaFact]
    public void TheSimulatorPanelListsThePartnersMessagesAndTakesTheChoicesOfTheSelectedOne()
    {
        ParticipantWorkspace workspace = Workspace(out ParticipantSession session, out _, out FakeSimulatorServer simulator);
        Assert.Equal(2, workspace.StepList.ItemsSource!.Cast<string>().Count());
        Assert.StartsWith("approval_decision from approver, canned payload, delay 0s, no failure", workspace.StepList.ItemsSource!.Cast<string>().First(), StringComparison.Ordinal);

        workspace.StepList.SelectedIndex = 1;
        TestHost.Pump();
        Assert.True(workspace.SetFailure(SimulatedFailure.Duplicate));
        workspace.StepList.SelectedIndex = 0;
        TestHost.Pump();
        Assert.True(workspace.SetFailure(SimulatedFailure.Timeout));
        workspace.PayloadBox.Text = "{\"decision\":\"approved\",\"comment\":\"ok\"}";
        Assert.True(workspace.ApplyPayload());
        workspace.PayloadBox.Text = "{\"decision\":\"approved\",\"extra\":true}";
        Assert.False(workspace.ApplyPayload());

        Assert.Equal(SimulatedFailure.Timeout, session.Simulator.Step("approval_decision")!.Failure);
        Assert.Equal(SimulatedFailure.Duplicate, session.Simulator.Step("documents_requested")!.Failure);
        Assert.False(session.Simulator.Step("approval_decision")!.IsCanned);

        simulator.Result = new SimulationResult(
            "sim_2",
            SimulationOutcome.Stalled,
            [new SimulationEvent(1, SimulationDirection.Received, "approval_decision", "approver", DeliveryState.TimedOut, 86_400)]);
        TestHost.Await(session.RunSimulationAsync(TestContext.Current.CancellationToken));
        TestHost.Pump();

        Assert.Equal(["approval_decision received from approver: Timed out at 86400s"], workspace.HistoryList.ItemsSource!.Cast<string>());
        Assert.Single(simulator.Requests);
    }

    [AvaloniaFact]
    public void TheProblemListHoldsTheServersContractProblemsAndTheConformanceAndTheHints()
    {
        ParticipantWorkspace workspace = Workspace(out ParticipantSession session, out FakeParticipantServer server, out _);
        server.ContractProblems = [new ContractProblem("bpm_contract.dead_end", "documents_requested", "from this message no path reaches an outcome")];
        server.ConformanceProblems = [new ContractProblem("bpm_contract.conformance_timeout", "wait_message", "wait node `wait_message` has no timeout branch")];

        TestHost.Await(session.CheckContractAsync(TestContext.Current.CancellationToken));
        TestHost.Await(session.CheckConformanceAsync(TestContext.Current.CancellationToken));
        TestHost.Pump();

        List<string> rows = [.. workspace.ProblemList.ItemsSource!.Cast<string>()];
        Assert.Contains("bpm_contract.dead_end (documents_requested): from this message no path reaches an outcome", rows);
        Assert.Contains(rows, row => row.StartsWith("bpm_contract.conformance_timeout:", StringComparison.Ordinal) && row.EndsWith("(wait_message)", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public void TheEffectListsShowTheBadgesAndThePreview()
    {
        ParticipantWorkspace workspace = Workspace(out ParticipantSession session, out FakeParticipantServer server, out _);
        server.Effects = ParticipantFixtures.EffectSummary();
        server.Preview = ParticipantFixtures.Preview();

        TestHost.Await(session.LoadEffectsAsync(TestContext.Current.CancellationToken));
        TestHost.Await(session.LoadPreviewAsync("run_3", TestContext.Current.CancellationToken));
        TestHost.Pump();

        List<string> rows = [.. workspace.EffectList.ItemsSource!.Cast<string>()];
        Assert.Contains(rows, row => row.StartsWith("send_message: partner_message (irreversible, external)", StringComparison.Ordinal));
        Assert.Contains("Effects of 3 steps. 1 undeclared.", rows);
        Assert.Contains(rows, row => row.Contains("undeclared: no reversal declared", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public void TheDiagramIsPaintedAndReadAloudAsTheArrowsOfTheOrderTable()
    {
        StateDiagramView view = new() { Diagram = StateDiagram.From(ParticipantFixtures.Contract()) };
        TestHost.Show(view, 800, 300);

        int width = (int)Math.Max(1, view.Bounds.Width);
        int height = (int)Math.Max(1, view.Bounds.Height);
        byte[] pixels = new byte[width * 4 * height];
        using RenderTargetBitmap target = new(new PixelSize(width, height));
        target.Render(view);
        IntPtr buffer = Marshal.AllocHGlobal(pixels.Length);
        try
        {
            target.CopyPixels(new PixelRect(0, 0, width, height), buffer, pixels.Length, width * 4);
            Marshal.Copy(buffer, pixels, 0, pixels.Length);
        }
        finally
        {
            Marshal.FreeHGlobal(buffer);
        }

        int blue = 0;
        for (int i = 0; i < pixels.Length; i += 4)
        {
            if (Math.Abs(pixels[i] - 0xEB) <= 12 && Math.Abs(pixels[i + 1] - 0x63) <= 12 && Math.Abs(pixels[i + 2] - 0x25) <= 12)
            {
                blue++;
            }
        }

        Assert.True(blue > 100, "the message states are not drawn");
        Assert.Equal(StateDiagramView.BoxWidth, StateDiagramView.BoxOf(view.Diagram.Nodes[0]).Width);
        Assert.Contains("start -> approval_request [buyer]", view.Diagram.ToText(), StringComparison.Ordinal);
    }
}
