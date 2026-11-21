using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.FlowDesigner;
using Spotledger.Client.FlowDesigner.Run;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The workspace a host shows: the four step-through controls, the sample-data box with the trigger's
/// hints, the node state list, the canvas badges, the history panel and the shortcut keys — all of it
/// without a pointer.
public sealed class WorkspaceTests
{

    [AvaloniaFact]

    public void TheWorkspaceOffersTheControlsWithTheRunLabelWords()
    {
        FlowRunSession session = Session(out FakeRunServer server, out FlowDocument document);
        RunWorkspace workspace = new(session);
        TestHost.Show(workspace, 1200, 800);
        TestHost.Pump();

        Assert.Equal(RunStrings.RunTest, workspace.RunTestButton.Content);
        Assert.Equal(RunStrings.Continue, workspace.ContinueButton.Content);
        Assert.Equal(RunStrings.Step, workspace.StepButton.Content);
        Assert.Equal(RunStrings.Stop, workspace.StopButton.Content);

        // Before a run there is nothing to step, continue or stop.
        Assert.False(workspace.StepButton.IsEnabled);
        Assert.False(workspace.ContinueButton.IsEnabled);
        Assert.False(workspace.StopButton.IsEnabled);
        Assert.True(workspace.RunTestButton.IsEnabled);
        Assert.Equal(document.Name, workspace.Session.Document.Name);
        Assert.NotEmpty(workspace.HintList.ItemsSource!.Cast<object>());
        Assert.Contains(RunStrings.NoRunYet, workspace.StateList.ItemsSource!.Cast<string>());
        Assert.Equal(RunStrings.SampleDataHelp, workspace.SampleDataBox.PlaceholderText);
        _ = server;
    }

    [AvaloniaFact]

    public void TheSampleDataBoxFeedsTheSessionAndTheHintsNameTheTriggerFields()
    {
        FlowRunSession session = Session(out _, out _);
        RunWorkspace workspace = new(session);
        TestHost.Show(workspace);
        TestHost.Pump();

        workspace.SampleDataBox.Text = RunFixtures.SampleJson;
        TestHost.Pump();

        Assert.Equal(RunFixtures.SampleJson, session.SampleData.Text, StringComparer.Ordinal);
        List<string> hints = [.. workspace.HintList.ItemsSource!.Cast<string>()];
        Assert.Contains(hints, hint => hint.StartsWith("path: Path", StringComparison.Ordinal));
        Assert.Contains(hints, hint => hint.StartsWith("payload:", StringComparison.Ordinal));
    }

    [AvaloniaFact]

    public void TheF5ShortcutRunsATestAndTheBadgesReachTheCanvas()
    {
        FlowRunSession session = Session(out FakeRunServer server, out _);
        RunWorkspace workspace = new(session);
        TestHost.Show(workspace);
        TestHost.Pump();
        workspace.SampleDataBox.Text = RunFixtures.SampleJson;
        TestHost.Pump();

        TestHost.Await(workspace.HandleKeyAsync(Key.F5, default));

        Assert.Single(server.Started);
        Assert.NotNull(session.Snapshot);
        Assert.Equal("Running", workspace.Canvas.BadgeText("webhook"));
        Assert.Equal("Pending", workspace.Canvas.BadgeText("mail"));
        Assert.True(workspace.StepButton.IsEnabled);
        Assert.True(workspace.ContinueButton.IsEnabled);
        Assert.True(workspace.StopButton.IsEnabled);
        Assert.Contains("started", workspace.StatusLine.Text!, StringComparison.Ordinal);
        List<string> states = [.. workspace.StateList.ItemsSource!.Cast<string>()];
        Assert.Contains(states, state => state.Contains("webhook", StringComparison.Ordinal)
            && state.Contains("Running", StringComparison.Ordinal));
    }

    [AvaloniaFact]

    public void TheStepShortcutAdvancesOneNodeAndTheHistoryAppearsWhenTheRunEnds()
    {
        FlowRunSession session = Session(out FakeRunServer server, out _);
        RunWorkspace workspace = new(session);
        TestHost.Show(workspace);
        TestHost.Pump();
        workspace.SampleDataBox.Text = RunFixtures.SampleJson;
        TestHost.Pump();
        server.Enqueue(RunFixtures.Snapshot("run_1", RunOutcome.Running,
            ("webhook", RunNodeStatus.Succeeded), ("document", RunNodeStatus.Running),
            ("http", RunNodeStatus.Pending), ("mail", RunNodeStatus.Pending)));
        server.Enqueue(RunFixtures.Snapshot("run_1", RunOutcome.Succeeded,
            ("webhook", RunNodeStatus.Succeeded), ("document", RunNodeStatus.Succeeded),
            ("http", RunNodeStatus.Succeeded), ("mail", RunNodeStatus.Succeeded)));
        server.Detail = RunFixtures.ScenarioDetail();

        TestHost.Await(workspace.HandleKeyAsync(Key.F5, default));
        TestHost.Await(workspace.HandleKeyAsync(Key.F10, default));

        Assert.Equal(RunStepMode.Step, server.Steps[0].Mode);
        Assert.Equal("Succeeded", workspace.Canvas.BadgeText("webhook"));

        TestHost.Await(workspace.HandleKeyAsync(Key.F8, default));

        Assert.Equal(RunStepMode.Continue, server.Steps[1].Mode);
        Assert.False(workspace.StepButton.IsEnabled);
        Assert.False(workspace.StopButton.IsEnabled);
        List<string> rows = [.. workspace.TimelineList.ItemsSource!.Cast<string>()];
        Assert.Equal(5, rows.Count);
        Assert.Contains("3. http (HTTP call): Failed", rows[2], StringComparison.Ordinal);
        Assert.Contains("Node timeline", workspace.HistoryText(), StringComparison.Ordinal);
    }

    [AvaloniaFact]

    public void TheStopShortcutStopsTheRun()
    {
        FlowRunSession session = Session(out FakeRunServer server, out _);
        RunWorkspace workspace = new(session);
        TestHost.Show(workspace);
        TestHost.Pump();
        workspace.SampleDataBox.Text = RunFixtures.SampleJson;
        TestHost.Pump();
        server.Detail = RunFixtures.ScenarioDetail();

        TestHost.Await(workspace.HandleKeyAsync(Key.F5, default));
        TestHost.Await(workspace.HandleKeyAsync(Key.F9, default));

        Assert.Equal(["run_1"], server.Stops);
        Assert.False(session.IsRunning);
        Assert.Contains("stopped", workspace.StatusLine.Text!, StringComparison.Ordinal);
    }

    [AvaloniaFact]

    public void AKeyThatIsNotACommandIsNotHandled()
    {
        FlowRunSession session = Session(out _, out _);
        RunWorkspace workspace = new(session);
        TestHost.Show(workspace);

        Assert.False(TestHost.Await(workspace.HandleKeyAsync(Key.Q, default)));
    }

    [AvaloniaFact]

    public async Task TheTimelineSelectionFillsTheInputAndOutputPanes()
    {
        FlowRunSession session = Session(out FakeRunServer server, out _);
        RunWorkspace workspace = new(session);
        TestHost.Show(workspace);
        TestHost.Pump();
        server.Detail = RunFixtures.ScenarioDetail();
        await session.OpenRunAsync("run_7", default);
        TestHost.Pump();

        workspace.TimelineList.SelectedIndex = 1;
        TestHost.Pump();

        List<string> input = [.. workspace.InputPaneList.ItemsSource!.Cast<string>()];
        Assert.Contains(input, line => line.Contains("token: [redacted", StringComparison.Ordinal));
        Assert.Contains(input, line => line.Contains("doctype", StringComparison.Ordinal));
        List<string> output = [.. workspace.OutputPaneList.ItemsSource!.Cast<string>()];
        Assert.Contains(output, line => line.Contains("SO-1", StringComparison.Ordinal));
    }

    private static FlowRunSession Session(out FakeRunServer server, out FlowDocument document)
    {
        server = new FakeRunServer();
        document = RunFixtures.ScenarioDocument();
        return new FlowRunSession(server, document, RunFixtures.Catalogue());
    }
}
