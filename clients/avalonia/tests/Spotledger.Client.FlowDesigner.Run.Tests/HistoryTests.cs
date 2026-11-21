using System;
using System.Collections.Generic;
using System.Linq;
using Spotledger.Client.FlowDesigner.Run;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The execution history: runs grouped by process and version under the organisation's run label,
/// a node timeline with every attempt, the retry count, the error branch and the server's durations,
/// and the redacted input and output panes with their markers kept visible.
public sealed class HistoryTests
{

    [Fact]

    public void RunsAreGroupedByProcessAndVersionUnderTheRunLabel()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.SetRuns(RunFixtures.Runs());

        Assert.Equal(3, history.Groups.Count);
        Assert.Equal("invoice_check", history.Groups[0].ProcessName);
        Assert.Equal(2, history.Groups[0].Version);
        Assert.Equal("Requests of invoice_check, version 2", history.Groups[0].Heading);

        RunHistoryGroup order = history.Groups[1];
        Assert.Equal("order_intake", order.ProcessName);
        Assert.Equal(4, order.Version);
        Assert.Single(order.Runs);

        RunHistoryGroup third = history.Groups[2];
        Assert.Equal(3, third.Version);
        Assert.Equal(["run_8", "run_7"], third.Runs.Select(run => run.RunId));
    }

    [Fact]

    public void AnOrganisationOverrideRenamesTheGroup()
    {
        RunHistory history = new(
            new RunStrings(RunLabels.Resolve("Application", "Applications")),
            new RunRedactor(),
            RunFixtures.Catalogue());

        history.SetRuns(RunFixtures.Runs());

        Assert.Equal("Applications of order_intake, version 3", history.Groups[2].Heading);
    }

    [Fact]

    public void TheTimelineShowsEveryAttemptWithItsRetryAndTheErrorBranch()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.Select(RunFixtures.ScenarioDetail());

        Assert.Equal(5, history.Timeline.Count);
        Assert.Equal(5, history.AttemptCount);

        RunTimelineRow webhook = history.Timeline[0];
        Assert.Equal("webhook", webhook.NodeId);
        Assert.Equal("Webhook", webhook.KindName);
        Assert.Equal("Succeeded", webhook.StatusText);
        Assert.Equal(1, webhook.Attempt);
        Assert.False(webhook.IsRetry);

        RunTimelineRow failed = history.Timeline[2];
        Assert.Equal("http", failed.NodeId);
        Assert.True(failed.ErrorBranch);
        Assert.Equal("Failed", failed.StatusText);
        Assert.Equal("bpm_integration.http_status", failed.ErrorCode);
        Assert.Equal("HTTP 500 from the peer", failed.ErrorMessage);
        Assert.Equal("1.0 s", failed.Duration);

        RunTimelineRow retry = history.Timeline[3];
        Assert.True(retry.IsRetry);
        Assert.Equal(2, retry.Attempts);
        Assert.Equal("Succeeded", retry.StatusText);
        Assert.Contains("attempt 2", retry.Text, StringComparison.Ordinal);
        Assert.Contains("retry", retry.Text, StringComparison.Ordinal);
        Assert.Contains("error branch", failed.Text, StringComparison.Ordinal);
    }

    [Fact]

    public void TheDurationsAreTheServersAndAreWholeUnits()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.Select(RunFixtures.ScenarioDetail());

        Assert.All(history.Timeline, row => Assert.NotNull(row.Duration));
        Assert.Equal("580 ms", history.Timeline[1].Duration);
        Assert.Equal("400 ms", history.Timeline[4].Duration);
    }

    [Fact]

    public void TheExchangePanesKeepTheServersMarkersVisible()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.Select(RunFixtures.ScenarioDetail());

        RunExchange document = history.Exchange("document", 1)!;
        Assert.Contains("[redacted:api]", document.InputText, StringComparison.Ordinal);
        Assert.Contains("[redacted:api]", string.Join('\n', document.InputLines), StringComparison.Ordinal);
        Assert.Contains("SO-1", document.OutputText, StringComparison.Ordinal);
        Assert.Contains("doctype", document.InputLines[1], StringComparison.Ordinal);
    }

    [Fact]

    public void AnEmptyPayloadSaysSoInsteadOfShowingNothing()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.Select(RunFixtures.ScenarioDetail());

        RunExchange firstHttpAttempt = history.Exchange("http", 1)!;
        Assert.Equal(RunStrings.NoOutput, firstHttpAttempt.OutputText);
        Assert.Equal([RunStrings.NoOutput], firstHttpAttempt.OutputLines);
    }

    [Fact]

    public void TheSavedTextFormCarriesTheTimelineAndThePanes()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());
        history.SetRuns(RunFixtures.Runs());
        history.Select(RunFixtures.ScenarioDetail());

        string text = history.ToText();

        Assert.Contains("Requests of order_intake, version 3", text, StringComparison.Ordinal);
        Assert.Contains("Request run_7, failed", text, StringComparison.Ordinal);
        Assert.Contains(RunStrings.TimelineHeading, text, StringComparison.Ordinal);
        Assert.Contains("1. webhook (Webhook): Succeeded, attempt 1, 120 ms", text, StringComparison.Ordinal);
        Assert.Contains("3. http (HTTP call): Failed, attempt 1, 1.0 s, error branch", text, StringComparison.Ordinal);
        Assert.Contains(RunStrings.InputPane, text, StringComparison.Ordinal);
        Assert.Contains(RunStrings.OutputPane, text, StringComparison.Ordinal);
        Assert.Contains("[redacted:api]", text, StringComparison.Ordinal);
    }

    [Fact]

    public void NoRunYetSaysWhatToDoInTheRunLabelWords()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.SetRuns([]);
        history.Select(null);

        Assert.Contains("Run a test to see the history", history.ToText(), StringComparison.Ordinal);
        Assert.Contains("requests", history.ToText(), StringComparison.OrdinalIgnoreCase);
        Assert.Equal(new RunStrings().NoTimeline, history.Heading);
    }

    [Fact]

    public void TheHeadingNamesTheRunWithTheLabelPair()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.Select(RunFixtures.ScenarioDetail());

        Assert.StartsWith("Request run_7", history.Heading, StringComparison.Ordinal);
        Assert.EndsWith("UTC", history.Heading, StringComparison.Ordinal);
    }

    [Fact]

    public void TheAttemptOrderIsTheServersTimelineNotTheListOrder()
    {
        RunDetail detail = RunFixtures.ScenarioDetail();
        List<RunNodeAttempt> shuffled = [.. detail.Attempts.Reverse()];
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());

        history.Select(new RunDetail(detail.Summary, shuffled));

        Assert.Equal(["webhook", "document", "http", "http", "mail"], history.Timeline.Select(row => row.NodeId));
        Assert.Equal([1, 2, 3, 4, 5], history.Timeline.Select(row => row.Order));
    }
}
