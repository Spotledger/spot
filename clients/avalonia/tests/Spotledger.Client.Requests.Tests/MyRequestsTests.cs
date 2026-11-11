using Avalonia.Headless.XUnit;
using Spotledger.Client.Api;
using Xunit;

namespace Spotledger.Client.Requests.Tests;

public sealed class MyRequestsTests
{
    private static RequestSummary Run(string id, DateTimeOffset? due, bool waitingForMe = true, string? waitingFor = null, RequestState state = RequestState.Running)
        => new(id, id[1..], "Claim " + id, "Expense", state, waitingForMe, waitingFor, due, Clock.Now.AddDays(-1));

    [Fact]
    public async Task A_slow_older_refresh_never_overwrites_the_view_chosen_later()
    {
        var gate = new TaskCompletionSource();
        var gateway = new FakeRequests();
        gateway.Mine[RequestFilter.WaitingForMe] = [Run("R1", null)];
        gateway.Mine[RequestFilter.Finished] = [Run("R9", null, state: RequestState.Finished)];
        gateway.ListGate = f => f == RequestFilter.WaitingForMe ? gate.Task : Task.CompletedTask;
        var model = new MyRequestsViewModel(gateway, Texts.English("Application", "Applications"));

        var slow = model.RefreshAsync(Clock.Now);
        await model.SelectFilterAsync(RequestFilter.Finished, Clock.Now);
        gate.SetResult();
        await slow;

        Assert.Equal(["R9"], model.Rows.Select(r => r.Summary.Id));
    }

    [Fact]
    public async Task Rows_carry_the_run_label_and_overdue_and_due_soon_badges()
    {
        var gateway = new FakeRequests();
        gateway.Mine[RequestFilter.WaitingForMe] =
        [
            Run("R3", null),
            Run("R2", Clock.Now.AddHours(2)),
            Run("R1", Clock.Now.AddHours(-1)),
            Run("R4", Clock.Now.AddDays(3)),
        ];
        var model = new MyRequestsViewModel(gateway, Texts.English("Application", "Applications"));

        await model.RefreshAsync(Clock.Now);

        Assert.Equal(["R1", "R2", "R4", "R3"], model.Rows.Select(r => r.Summary.Id));
        Assert.Equal("Application 1", model.Rows[0].Reference);
        Assert.Equal(("Overdue", Urgency.Overdue), (model.Rows[0].Badge, model.Rows[0].Urgency));
        Assert.Equal(("Due soon", Urgency.DueSoon), (model.Rows[1].Badge, model.Rows[1].Urgency));
        Assert.Null(model.Rows[2].Badge);
        Assert.Null(model.Rows[3].Badge);
        Assert.Equal("My Applications", model.Title);
    }

    [Fact]
    public async Task A_finished_run_is_never_overdue()
    {
        var gateway = new FakeRequests();
        gateway.Mine[RequestFilter.Finished] = [Run("R1", Clock.Now.AddDays(-5), state: RequestState.Finished)];
        var model = new MyRequestsViewModel(gateway, Texts.English());

        await model.SelectFilterAsync(RequestFilter.Finished, Clock.Now);

        Assert.Equal(Urgency.None, Assert.Single(model.Rows).Urgency);
    }

    [Fact]
    public async Task The_three_views_ask_the_server_for_their_own_filter()
    {
        var gateway = new FakeRequests();
        gateway.Mine[RequestFilter.WaitingForOthers] = [Run("R9", null, waitingForMe: false, waitingFor: "Ben")];
        var model = new MyRequestsViewModel(gateway, Texts.English());

        await model.RefreshAsync(Clock.Now);
        Assert.True(model.IsEmpty);
        await model.SelectFilterAsync(RequestFilter.WaitingForOthers, Clock.Now);

        Assert.Equal(["list WaitingForMe", "list WaitingForOthers"], gateway.Calls);
        Assert.Equal("Waiting for Ben", Assert.Single(model.Rows).Waiting);
        Assert.Equal(["Waiting for me", "Waiting for others", "Finished"], Enum.GetValues<RequestFilter>().Select(model.FilterLabel));
    }

    [Fact]
    public async Task A_failed_load_shows_a_plain_sentence_and_keeps_nothing_stale()
    {
        var model = new MyRequestsViewModel(new ThrowingRequests(), Texts.English());

        await model.RefreshAsync(Clock.Now);

        Assert.Equal("That did not work. Nothing was changed.", model.Error);
        Assert.False(model.IsEmpty);
    }

    [Fact]
    public async Task Arabic_localises_the_texts_and_flows_right_to_left()
    {
        var model = new MyRequestsViewModel(new FakeRequests(), Texts.For("ar"));

        await model.RefreshAsync(Clock.Now);

        Assert.Equal("الطلبات الخاصة بي", model.Title);
        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, model.Text.FlowDirection);
        Assert.Equal("بانتظاري", model.FilterLabel(RequestFilter.WaitingForMe));
    }

    [AvaloniaFact]
    public async Task The_view_shows_the_rows_badges_and_flow_direction()
    {
        var gateway = new FakeRequests();
        gateway.Mine[RequestFilter.WaitingForMe] = [Run("R1", Clock.Now.AddHours(-1))];
        var model = new MyRequestsViewModel(gateway, Texts.For("ar"));
        await model.RefreshAsync(Clock.Now);

        var view = new MyRequestsView(model, () => Clock.Now);
        var window = TestHost.Show(view);

        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, view.FlowDirection);
        var texts = AllText(view);
        Assert.Contains("متأخر", texts);
        window.Close();
    }

    internal static List<string> AllText(Avalonia.Visual root)
    {
        var result = new List<string>();
        foreach (var visual in Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(root))
        {
            if (visual is Avalonia.Controls.TextBlock { Text: { Length: > 0 } text })
            {
                result.Add(text);
            }
        }

        return result;
    }

    private sealed class ThrowingRequests : IRequestsGateway
    {
        private static ApiRequestException Fail() => new(ApiError.TransportFailure(0, "down"));

        public Task<EntryInfo> GetEntryInfoAsync(string recordType, CancellationToken cancellationToken) => throw Fail();
        public Task<StartedRequest> StartAsync(string processId, CancellationToken cancellationToken) => throw Fail();
        public Task<StepInfo> GetStepAsync(string taskId, CancellationToken cancellationToken) => throw Fail();
        public Task CompleteStepAsync(string taskId, CancellationToken cancellationToken) => throw Fail();
        public Task<IReadOnlyList<RequestSummary>> ListMineAsync(RequestFilter filter, CancellationToken cancellationToken) => throw Fail();
        public Task<RequestDetail> GetRequestAsync(string requestId, CancellationToken cancellationToken) => throw Fail();
        public Task UndoAsync(string requestId, CancellationToken cancellationToken) => throw Fail();
        public Task CancelAsync(string requestId, string? reason, CancellationToken cancellationToken) => throw Fail();
    }
}
