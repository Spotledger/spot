using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class LookupTests
{
    [AvaloniaFact]
    public async Task DebounceCancelsSupersededQueriesAndRejectsLateResponses()
    {
        var clock = new ManualLookupDelay();
        var source = new PendingLookupSource();
        var input = new SpotLookupBox { Source = source, Delay = clock };
        TestHost.Show(input);
        input.SearchText = "a";
        Assert.Empty(source.Calls);
        var firstDelay = clock.Last;
        input.SearchText = "ab";
        Assert.True(firstDelay.Token.IsCancellationRequested);
        clock.Fire();
        Assert.Single(source.Calls);
        Assert.True(input.IsLoading);
        var old = source.Calls[0];
        input.SearchText = "abc";
        Assert.True(old.Token.IsCancellationRequested);
        clock.Fire();
        source.Calls[1].Result.SetResult([new LookupItem("new", "New", "description")]);
        await input.PendingSearch;
        old.Result.SetResult([new LookupItem("old", "Old")]);
        Assert.Equal("new", Assert.Single(input.Items).Id);
        Assert.False(input.IsLoading);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaTheory]
    [InlineData(Key.Enter, false, "one")]
    [InlineData(Key.Tab, false, null)]
    [InlineData(Key.Tab, true, "two")]
    [InlineData(Key.Escape, true, null)]
    public async Task KeyboardAcceptanceTable(Key finish, bool navigate, string? selected)
    {
        var input = new SpotLookupBox { Source = new StaticLookupSource(), Delay = new ImmediateLookupDelay() };
        TestHost.Show(input);
        input.SearchText = "o";
        await input.PendingSearch;
        input.Close(false);
        input.HandleKey(Key.Down);
        if (navigate) input.HandleKey(Key.Down);
        input.HandleKey(finish);
        Assert.Equal(selected, input.SelectedItem?.Id);
        Assert.False(input.IsOpen);
    }

    [AvaloniaFact]
    public async Task ArrowNavigationClampsAndUpReturnsToTheFirstCandidate()
    {
        var input = new SpotLookupBox { Source = new StaticLookupSource(), Delay = new ImmediateLookupDelay() };
        var window = TestHost.Show(input);
        input.SearchText = "query";
        await input.PendingSearch;
        input.HandleKey(Key.Up);
        Assert.Equal(0, input.HighlightedIndex);
        input.HandleKey(Key.Down);
        input.HandleKey(Key.Down);
        Assert.Equal(1, input.HighlightedIndex);
        input.HandleKey(Key.Up);
        input.HandleKey(Key.Enter);
        Assert.Equal("one", input.SelectedItem!.Id);
        window.Close();
    }

    [AvaloniaFact]
    public async Task SavedMissingSelectionHasAnExplicitStaleState()
    {
        var input = new SpotLookupBox { Source = new StaticLookupSource() };
        await input.SetSelectionAsync(new LookupItem("missing", "Saved"));
        Assert.True(input.IsStale);
        Assert.Equal("missing", input.SelectedItem!.Id);
        Assert.Contains("no longer", input.StatusText);
    }
}

internal sealed class ImmediateLookupDelay : ILookupDelay
{
    public Task WaitAsync(TimeSpan delay, CancellationToken token) => Task.CompletedTask;
}
internal sealed class ManualLookupDelay : ILookupDelay
{
    public (TaskCompletionSource Result, CancellationToken Token) Last { get; private set; }
    public Task WaitAsync(TimeSpan delay, CancellationToken token)
    {
        Assert.Equal(TimeSpan.FromMilliseconds(200), delay);
        var completion = new TaskCompletionSource();
        token.Register(() => completion.TrySetCanceled(token));
        Last = (completion, token);
        return completion.Task;
    }
    public void Fire() => Last.Result.SetResult();
}
internal sealed class PendingLookupSource : ILookupSource
{
    public List<(TaskCompletionSource<IReadOnlyList<LookupItem>> Result, CancellationToken Token)> Calls { get; } = [];
    public Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken)
    {
        var completion = new TaskCompletionSource<IReadOnlyList<LookupItem>>();
        Calls.Add((completion, cancellationToken));
        return completion.Task;
    }
    public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken) => Task.FromResult<LookupItem?>(null);
}
internal sealed class StaticLookupSource : ILookupSource
{
    public Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<LookupItem>>([new("one", "One", "First"), new("two", "Two", "Second")]);
    public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken) => Task.FromResult<LookupItem?>(null);
}
