using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Browser.Shell.Tests;

public sealed class UpdateTests
{
    private static UpdateStateMachine Ready(bool edits)
    {
        var machine = new UpdateStateMachine("B1");
        machine.SetUnsavedEdits(edits);
        machine.OnUpdateFound("B2");
        machine.OnStaged("B2");
        return machine;
    }

    [Fact]
    public void PromptIsOfferedWhenTheUpdateIsReadyAndNothingIsUnsaved()
    {
        var machine = Ready(edits: false);
        Assert.Equal(UpdateState.UpdateReady, machine.State);
        Assert.True(machine.PromptVisible);
    }

    [Fact]
    public void AnUpdateFoundDuringAnUnsavedEditWaitsAndIsAppliedAfter()
    {
        var started = new List<string>();
        var machine = new UpdateStateMachine("B1");
        machine.ApplyStarted += started.Add;
        machine.SetUnsavedEdits(true);
        machine.OnUpdateFound("B2");
        machine.OnStaged("B2");

        Assert.False(machine.PromptVisible);
        Assert.Equal(ApplyOutcome.Deferred, machine.RequestApply());
        Assert.Equal(UpdateState.UpdateReady, machine.State);
        Assert.Empty(started);

        machine.SetUnsavedEdits(false);
        Assert.Equal(UpdateState.Applying, machine.State);
        Assert.Equal(["B2"], started);

        machine.OnActivated("B2");
        Assert.Equal(UpdateState.Idle, machine.State);
        Assert.Equal("B2", machine.CurrentBuild);
    }

    [Fact]
    public void ApplyIsNeverStartedWhileEditsAreUnsaved()
    {
        var machine = Ready(edits: true);
        Assert.Equal(ApplyOutcome.Deferred, machine.RequestApply());
        machine.SetUnsavedEdits(true);
        Assert.Equal(UpdateState.UpdateReady, machine.State);
    }

    [Fact]
    public void AnAppThatNeverReportsIsTreatedAsHavingEdits()
    {
        var machine = new UpdateStateMachine("B1");
        machine.OnUpdateFound("B2");
        machine.OnStaged("B2");
        Assert.False(machine.PromptVisible);
        Assert.Equal(ApplyOutcome.Deferred, machine.RequestApply());
        Assert.Equal(UpdateState.UpdateReady, machine.State);
    }

    [Fact]
    public void TheRequestIsForgottenWhenANewerBuildReplacesTheWaitingOne()
    {
        var machine = Ready(edits: true);
        machine.RequestApply();
        machine.OnUpdateFound("B3");
        machine.OnStaged("B3");
        machine.SetUnsavedEdits(false);
        Assert.Equal(UpdateState.UpdateReady, machine.State);
        Assert.Equal("B3", machine.PendingBuild);
    }

    [Fact]
    public void DismissHidesThePromptButKeepsTheUpdate()
    {
        var machine = Ready(edits: false);
        machine.Dismiss();
        Assert.False(machine.PromptVisible);
        Assert.Equal(UpdateState.UpdateReady, machine.State);
        Assert.Equal(ApplyOutcome.Started, machine.RequestApply());
    }

    [Fact]
    public void IgnoresTheRunningBuildStaleEventsAndEventsWhileApplying()
    {
        var machine = Ready(edits: false);
        machine.RequestApply();
        machine.OnUpdateFound("B3");
        Assert.Equal("B2", machine.PendingBuild);
        var idle = new UpdateStateMachine("B1");
        idle.OnUpdateFound("B1");
        idle.OnStaged("B9");
        Assert.Equal(UpdateState.Idle, idle.State);
        Assert.Equal(ApplyOutcome.NotReady, idle.RequestApply());
    }

    [Fact]
    public void AFailedDownloadReturnsToIdle()
    {
        var machine = new UpdateStateMachine("B1");
        machine.OnUpdateFound("B2");
        machine.OnStageFailed("B2");
        Assert.Equal(UpdateState.Idle, machine.State);
        Assert.Null(machine.PendingBuild);
    }

    [Fact]
    public void AgreesWithTheGoldenVectors()
    {
        using var document = JsonDocument.Parse(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "Golden", "vectors.json")));
        foreach (var v in document.RootElement.GetProperty("update").EnumerateArray())
        {
            string name = v.GetProperty("name").GetString()!;
            var machine = new UpdateStateMachine(v.GetProperty("current").GetString()!);
            foreach (var step in v.GetProperty("steps").EnumerateArray())
            {
                string action = step.GetProperty("do").GetString()!;
                string? result = null;
                switch (action)
                {
                    case "edits": machine.SetUnsavedEdits(step.GetProperty("value").GetBoolean()); break;
                    case "found": machine.OnUpdateFound(step.GetProperty("build").GetString()!); break;
                    case "staged": machine.OnStaged(step.GetProperty("build").GetString()!); break;
                    case "failed": machine.OnStageFailed(step.GetProperty("build").GetString()!); break;
                    case "dismiss": machine.Dismiss(); break;
                    case "activated": machine.OnActivated(step.GetProperty("build").GetString()!); break;
                    case "apply":
                        result = machine.RequestApply() switch { ApplyOutcome.NotReady => "not-ready", ApplyOutcome.Started => "started", _ => "deferred" };
                        break;
                    default: throw new InvalidOperationException(action);
                }

                var expect = step.GetProperty("expect");
                string state = machine.State switch
                {
                    UpdateState.Idle => "idle",
                    UpdateState.UpdateFound => "update-found",
                    UpdateState.UpdateReady => "update-ready",
                    _ => "applying",
                };
                string at = name + " / " + action;
                Assert.True(expect.GetProperty("state").GetString() == state, at + ": state " + state);
                Assert.True(expect.GetProperty("prompt").GetBoolean() == machine.PromptVisible, at + ": prompt");
                if (expect.TryGetProperty("result", out var expected))
                {
                    Assert.True(expected.GetString() == result, at + ": result " + result);
                }
            }
        }
    }
}
