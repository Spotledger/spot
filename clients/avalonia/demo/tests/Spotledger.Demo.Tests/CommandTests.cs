using Spotledger.Demo.Mock;
using Spotledger.Demo.Mock.Contracts;
using Xunit;

namespace Spotledger.Demo.Tests;

public sealed class CommandTests
{
    [Fact]
    public async Task EveryRouteUsesSameHandlerAndContextAndBlockedReason()
    {
        var backend = new MockBackend(); var home = (await backend.GetHomeAsync()).Value!;
        var registry = CommandRegistry.Create(home); var dispatcher = new CommandDispatcher(backend, registry);
        var contexts = new List<CommandContext>();
        dispatcher.Register("create.order", ctx => { contexts.Add(ctx); return Task.CompletedTask; });
        dispatcher.Register("document.post", _ => throw new InvalidOperationException("Blocked handler must never run"));
        var context = new CommandContext(home.Company.Id, home.Company.Establishments[0].Id, MockBackend.RecordId, "buy");
        foreach (var route in Enum.GetValues<CommandRoute>())
        {
            Assert.True((await dispatcher.ExecuteAsync("create.order", context, route)).Executed);
            var blocked = await dispatcher.ExecuteAsync("document.post", context, route);
            Assert.False(blocked.Executed);
            Assert.Equal("Goods receipt is required before this invoice can proceed.", blocked.Availability.Reason);
            Assert.Equal("create.receipt", blocked.Availability.ResolutionCommandId);
        }
        Assert.Equal(Enum.GetValues<CommandRoute>().Length, contexts.Count);
        Assert.All(contexts, ctx => Assert.Same(context, ctx));
    }
    [Fact]
    public async Task DuplicateTriggerIsIgnoredUntilOriginalCompletes()
    {
        var backend = new MockBackend(); var home = (await backend.GetHomeAsync()).Value!;
        var dispatcher = new CommandDispatcher(backend, CommandRegistry.Create(home));
        var gate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously); var calls = 0;
        dispatcher.Register("create.order", async _ => { calls++; await gate.Task; });
        var context = new CommandContext(home.Company.Id, home.Company.Establishments[0].Id, null, "home");
        var first = dispatcher.ExecuteAsync("create.order", context, CommandRoute.Menu);
        var second = await dispatcher.ExecuteAsync("create.order", context, CommandRoute.Launcher);
        Assert.True(second.Duplicate); Assert.Equal(1, calls);
        gate.SetResult(); Assert.True((await first).Executed);
        Assert.True((await dispatcher.ExecuteAsync("create.order", context, CommandRoute.Shortcut)).Executed);
        Assert.Equal(2, calls);
    }
    [Fact]
    public async Task AllAvailabilityStatesComeFromBackend()
    {
        var backend = new MockBackend(); var home = (await backend.GetHomeAsync()).Value!;
        var context = new CommandContext(home.Company.Id, home.Company.Establishments[0].Id, null, "home");
        Assert.Equal(AvailabilityState.Available, (await backend.GetAvailabilityAsync("create.order", context)).Value!.State);
        Assert.Equal(AvailabilityState.NeedsContext, (await backend.GetAvailabilityAsync("document.post", context)).Value!.State);
        Assert.Equal(AvailabilityState.Restricted, (await backend.GetAvailabilityAsync("tools.admin", context)).Value!.State);
        Assert.Equal(AvailabilityState.Blocked, (await backend.GetAvailabilityAsync("document.post", context with { RecordId = MockBackend.RecordId })).Value!.State);
    }
    [Fact]
    public async Task MergePatchHonorsVersionAndTypedFieldPathsAndReset()
    {
        var backend = new MockBackend();
        var invalid = await backend.PatchRecordAsync(MockBackend.RecordId, 1, new Dictionary<string, string?> { ["amount"] = "1" });
        Assert.Equal("/note", Assert.Single(invalid.Error!.FieldErrors).Path);
        var saved = await backend.PatchRecordAsync(MockBackend.RecordId, 1, new Dictionary<string, string?> { ["note"] = "Changed" });
        Assert.Equal(2, saved.Value!.Version); Assert.Equal("2", saved.Version); Assert.Equal("demo.note.save", saved.CommandId);
        var stale = await backend.PatchRecordAsync(MockBackend.RecordId, 1, new Dictionary<string, string?> { ["note"] = "Stale" });
        Assert.Equal("document.version_conflict", stale.Error!.Code);
        var reset = (await backend.ResetAsync()).Value!;
        Assert.Equal("Seed note", reset.Records[0].Note); Assert.Equal(1, reset.Records[0].Version);
        Assert.Equal(2, reset.Company.Establishments.Count);
        Assert.All(reset.Kpis, kpi => { Assert.NotEmpty(kpi.Source); Assert.Equal(reset.Now, kpi.AsOf); });
    }
    [Fact]
    public async Task ClockAndRoleFixturesAreDeterministic()
    {
        var clock = new FixedClock(new DateTimeOffset(2026, 1, 1, 0, 0, 0, TimeSpan.Zero));
        var backend = new MockBackend(clock);
        foreach (var role in Enum.GetValues<DemoRole>())
        {
            var first = (await backend.SwitchRoleAsync(role)).Value!;
            var second = (await backend.SwitchRoleAsync(role)).Value!;
            Assert.Equal(first.User, second.User); Assert.Equal(first.Tasks, second.Tasks); Assert.Equal(first.Now, clock.Now);
            Assert.Equal(first.Tasks.Count, first.Tasks.Select(t => t.Id).Distinct().Count());
            Assert.All(first.Tasks, task => Assert.Equal(first.User.Name, task.Owner));
        }
        clock.Advance(TimeSpan.FromHours(1)); Assert.Equal(clock.Now, (await backend.GetHomeAsync()).Value!.Now);
    }
}
