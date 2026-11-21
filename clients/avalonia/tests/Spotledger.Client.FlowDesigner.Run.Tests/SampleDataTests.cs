using System;
using System.Collections.Generic;
using System.Linq;
using Spotledger.Client.FlowDesigner;
using Spotledger.Client.FlowDesigner.Run;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The sample-data editor: JSON with the hint set the trigger's own schema gives, and a check that
/// says whether a test may run — a key the trigger does not deliver is advice, never a refusal.
public sealed class SampleDataTests
{

    [Fact]

    public void TheHintsComeFromTheTriggersOwnSchema()
    {
        SampleDataEditor editor = new(RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());

        Assert.NotNull(editor.Trigger);
        Assert.Equal("webhook", editor.Trigger!.Id);

        List<string> keys = [.. editor.Hints.Select(hint => hint.Key)];
        Assert.Contains("path", keys);
        Assert.Contains("payload", keys);
        Assert.Contains("headers", keys);
        Assert.DoesNotContain("trigger", keys);

        SampleDataHint path = editor.Hints.First(hint => hint.Key == "path");
        Assert.True(path.Required);
        Assert.Contains("Path", path.Text, StringComparison.Ordinal);
        Assert.Contains("required", path.Text, StringComparison.Ordinal);

        Assert.True(editor.IsTriggerKey("payload"));
        Assert.True(editor.IsTriggerKey("path"));
        Assert.False(editor.IsTriggerKey("customer"));
    }

    [Fact]

    public void ValidSampleDataIsReadyForATestRun()
    {
        SampleDataEditor editor = new(RunFixtures.ScenarioDocument(), RunFixtures.Catalogue()) { Text = RunFixtures.SampleJson };

        SampleDataCheck check = editor.Check();

        Assert.True(check.Ok);
        Assert.Empty(check.UnknownKeys);
        Assert.Empty(check.Problems);
        Assert.Equal(RunStrings.SampleDataAccepted, editor.Summary());
        Assert.Null(editor.FirstProblem());
    }

    [Fact]

    public void AKeyTheTriggerDoesNotDeliverIsAHintAndTheSampleStillRuns()
    {
        SampleDataEditor editor = new(RunFixtures.ScenarioDocument(), RunFixtures.Catalogue())
        {
            Text = """{"path":"/hooks/orders","payload":{},"customer":"ACME"}""",
        };

        SampleDataCheck check = editor.Check();

        Assert.True(check.Ok);
        Assert.Equal(["customer"], check.UnknownKeys);
        SampleDataProblem problem = Assert.Single(check.Problems);
        Assert.True(problem.Hint);
        Assert.Equal("bpm.run.sample.unknown_key", problem.Code);
        Assert.Contains("customer", problem.Message, StringComparison.Ordinal);
        Assert.Contains("1 key is not part of the trigger", editor.Summary(), StringComparison.Ordinal);
    }

    [Fact]

    public void InvalidJsonIsRefusedWithItsPosition()
    {
        SampleDataEditor editor = new(RunFixtures.ScenarioDocument(), RunFixtures.Catalogue()) { Text = "{\"path\": }" };

        SampleDataCheck check = editor.Check();

        Assert.False(check.Ok);
        Assert.Contains(check.Problems, problem => problem.Code == "bpm.run.sample.invalid_json");
        SampleDataProblem first = check.Problems[0];
        Assert.True(first.Line >= 1);
        Assert.Contains("line 1", first.Message, StringComparison.Ordinal);
        Assert.NotNull(editor.FirstProblem());
    }

    [Fact]

    public void ASampleThatIsNotAnObjectIsRefused()
    {
        SampleDataEditor editor = new(RunFixtures.ScenarioDocument(), RunFixtures.Catalogue()) { Text = "[1, 2]" };

        SampleDataCheck check = editor.Check();

        Assert.False(check.Ok);
        Assert.Equal("bpm.run.sample.not_an_object", check.Problems[0].Code);
        Assert.Equal(RunStrings.SampleDataNotObject, check.Problems[0].Message);
    }

    [Fact]

    public void AMissingRequiredKeyIsHintedAndStillRuns()
    {
        SampleDataEditor editor = new(RunFixtures.ScenarioDocument(), RunFixtures.Catalogue()) { Text = "{}" };

        SampleDataCheck check = editor.Check();

        Assert.True(check.Ok);
        Assert.Contains(check.Problems, problem => problem.Code == "bpm.run.sample.missing_key" && problem.Hint);
    }

    [Fact]

    public void AnEmptySampleIsNotValidJson()
    {
        SampleDataEditor editor = new(RunFixtures.ScenarioDocument(), RunFixtures.Catalogue()) { Text = "   " };

        Assert.False(editor.Check().Ok);
    }
}
