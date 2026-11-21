using Avalonia.Headless.XUnit;
using Xunit;

namespace Spotledger.Client.Requests.Tests;

public sealed class RequestPageTests
{
    private static readonly RequestSummary Summary = new("R-1", "12", "Taxi claim", "Expense", RequestState.Running, false, "Ben", null, Clock.Now);

    private static RequestDetail Detail(ActionState undo, ActionState cancel, params ConversationInfo[] conversations)
        => new(
            Summary,
            [
                new TimelineStep("Enter details", TimelineState.Done, "Ama", Clock.Now),
                new TimelineStep("Manager approval", TimelineState.Current, "Ben", null),
                new TimelineStep("Pay", TimelineState.Upcoming, null, null),
            ],
            [new RecordLink("expense", "EXP-9", "Expense EXP-9")],
            [new TaskItem("T-2", "Approve the claim", "Ben", null, true)],
            [new CommentItem("Ama", "Receipt attached.", Clock.Now)],
            conversations,
            undo,
            cancel);

    private static ConversationInfo Conversation(ConversationCondition condition = ConversationCondition.None) => new(
        "Supplier portal",
        [
            new ConversationMessage(true, "Please confirm the order.", Clock.Now, DeliveryState.Queued),
            new ConversationMessage(true, "Second try.", Clock.Now, DeliveryState.Sent),
            new ConversationMessage(true, "Third.", Clock.Now, DeliveryState.Delivered),
            new ConversationMessage(true, "Fourth.", Clock.Now, DeliveryState.Read),
            new ConversationMessage(true, "Fifth.", Clock.Now, DeliveryState.Failed),
            new ConversationMessage(false, "Confirmed.", Clock.Now, DeliveryState.Read),
        ],
        condition,
        Clock.Now.AddDays(-2));

    private static async Task<RequestPageViewModel> LoadAsync(FakeRequests gateway, RequestText? text = null)
    {
        var model = new RequestPageViewModel(gateway, "R-1", text ?? Texts.English());
        await model.LoadAsync();
        return model;
    }

    [Fact]
    public async Task The_page_shows_timeline_records_tasks_and_comments_under_the_run_label()
    {
        var gateway = new FakeRequests();
        gateway.Details["R-1"] = Detail(ActionState.Ok, ActionState.Ok);

        var model = await LoadAsync(gateway, Texts.English("Application", "Applications"));

        Assert.Equal("Application 12", model.Title);
        Assert.Equal("In progress", model.StateText);
        Assert.Equal(["Done", "Now", "Later"], model.Timeline.Select(t => t.StateText));
        Assert.Equal("Cancel Application", model.Cancel!.Label);
        Assert.Single(model.Detail!.Records);
        Assert.Single(model.Detail.Tasks);
        Assert.Single(model.Detail.Comments);
    }

    [Fact]
    public async Task The_conversation_panel_shows_the_last_message_and_a_delivery_state_per_message()
    {
        var gateway = new FakeRequests();
        gateway.Details["R-1"] = Detail(ActionState.Ok, ActionState.Ok, Conversation());

        var model = await LoadAsync(gateway);

        var panel = Assert.Single(model.Conversations);
        Assert.Equal("Confirmed.", panel.Last!.Message.Text);
        Assert.Equal(
            ["Waiting to be sent", "Sent", "Delivered", "Read", "Could not be sent", null],
            panel.Lines.Select(l => l.DeliveryText));
        Assert.Null(panel.Condition);
    }

    [Fact]
    public async Task A_stuck_conversation_and_an_unreachable_one_are_explained_in_plain_language()
    {
        var gateway = new FakeRequests();
        gateway.Details["R-1"] = Detail(ActionState.Ok, ActionState.Ok, Conversation(ConversationCondition.Stuck), Conversation(ConversationCondition.Unreachable));

        var model = await LoadAsync(gateway);

        Assert.Equal("No answer from Supplier portal since 2026-10-21 09:00:00Z. It may need a nudge.", model.Conversations[0].Condition);
        Assert.Equal("Supplier portal cannot be reached right now. Messages are kept and sent when they can be.", model.Conversations[1].Condition);
    }

    [Fact]
    public async Task A_blocked_undo_is_disabled_with_the_reason_and_the_blockers_and_never_calls_the_server()
    {
        var gateway = new FakeRequests();
        gateway.Details["R-1"] = Detail(
            new ActionState(false, "A payment has already left the bank.", ["Payment PAY-4 is posted", "The supplier has been paid"]),
            ActionState.Ok);

        var model = await LoadAsync(gateway);

        Assert.False(model.Undo!.IsEnabled);
        Assert.Equal("A payment has already left the bank.", model.Undo.DisabledReason);
        Assert.Equal(["Payment PAY-4 is posted", "The supplier has been paid"], model.Undo.Blockers);
        Assert.False(await model.UndoAsync());
        Assert.Empty(gateway.Calls);
    }

    [Fact]
    public async Task Cancel_runs_when_allowed_and_the_page_reloads()
    {
        var gateway = new FakeRequests();
        gateway.Details["R-1"] = Detail(ActionState.Ok, ActionState.Ok);

        var model = await LoadAsync(gateway);

        Assert.True(await model.CancelAsync("Not needed"));
        Assert.Equal(["cancel R-1"], gateway.Calls);
    }

    [Fact]
    public async Task A_network_failure_on_cancel_shows_a_plain_sentence_and_no_raw_text()
    {
        var gateway = new FakeRequests { CancelThrowsNetwork = true };
        gateway.Details["R-1"] = Detail(ActionState.Ok, ActionState.Ok);
        var model = await LoadAsync(gateway);

        Assert.False(await model.CancelAsync(null));
        Assert.NotNull(model.Error);
        Assert.DoesNotContain("SECRET", model.Error, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public async Task The_view_shows_the_conversation_delivery_states_and_disables_Undo_with_its_blockers()
    {
        var gateway = new FakeRequests();
        gateway.Details["R-1"] = Detail(
            new ActionState(false, "A payment has already left the bank.", ["Payment PAY-4 is posted"]),
            ActionState.Ok,
            Conversation(ConversationCondition.Unreachable));
        var model = await LoadAsync(gateway);

        var window = TestHost.Show(new RequestPageView(model));

        var texts = MyRequestsTests.AllText(window);
        Assert.Contains(texts, t => t.Contains("(Delivered)", StringComparison.Ordinal));
        Assert.Contains(texts, t => t.Contains("(Could not be sent)", StringComparison.Ordinal));
        Assert.Contains("Payment PAY-4 is posted", texts);
        var undo = Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(window).OfType<Avalonia.Controls.Button>().Single(b => (string?)b.Content == "Undo");
        Assert.False(undo.IsEnabled);
        window.Close();
    }
}
