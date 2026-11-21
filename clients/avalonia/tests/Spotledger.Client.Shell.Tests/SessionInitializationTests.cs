using Avalonia.Headless.XUnit;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

public sealed class SessionInitializationTests
{
    [Fact]
    public async Task CanceledSignInDropsThePasswordAndCompletesTheCommand()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Handler.LoginResponder = _ => throw new OperationCanceledException();

        await h.SignInAsync();

        Assert.Empty(h.Login.Password);
        Assert.False(h.Login.SignInCommand.IsRunning);
        Assert.False(h.Connection.Tokens.HasToken);
    }

    [Fact]
    public async Task CanceledLogoutStillClearsTheLocalSession()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        await h.SignInAsync();
        Assert.Single(h.Shell.Tabs);
        h.Handler.LogoutResponder = _ => throw new OperationCanceledException();

        h.Shell.LogoutCommand.Execute(null);
        await h.Shell.LogoutCommand.ExecutionTask;

        Assert.True(h.App.ShowLogin);
        Assert.Empty(h.Shell.Tabs);
        Assert.False(h.Connection.Tokens.HasToken);
    }

    [AvaloniaFact]
    public async Task DisposingDuringPostLoginInitializationLeavesNoUnhandledEventFailure()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Handler.BeforeResponseAsync = async (request, cancellationToken) =>
        {
            if (request.RequestUri!.AbsolutePath == "/api/v1/auth/session")
            {
                started.SetResult();
                await Task.Delay(Timeout.Infinite, cancellationToken);
            }
        };

        h.Login.User = "ayesha@acme.test";
        h.Login.Password = "test-passphrase";
        h.Login.SignInCommand.Execute(null);
        await started.Task.WaitAsync(TimeSpan.FromSeconds(5), TestContext.Current.CancellationToken);
        Assert.False(h.App.SessionInitialization.IsCompleted);
        h.Connection.Dispose();

        await h.Login.SignInCommand.ExecutionTask;
        await h.App.SessionInitialization;
        Assert.True(h.App.SessionInitialization.IsCompletedSuccessfully);
        Assert.Empty(h.Shell.Tabs);
        Assert.Null(h.Shell.SessionError);
    }

    [AvaloniaTheory]
    [InlineData("transport")]
    [InlineData("unexpected")]
    [InlineData("disposed")]
    public async Task PostLoginFailureIsHandledAndDisplayedWhenAppropriate(string kind)
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Handler.SessionResponder = _ => throw kind switch
        {
            "transport" => new HttpRequestException("Connection reset"),
            "disposed" => new ObjectDisposedException("connection"),
            _ => new InvalidDataException("Unexpected response"),
        };

        await h.SignInAsync();

        Assert.True(h.App.SessionInitialization.IsCompletedSuccessfully);
        Assert.Equal(kind != "disposed", h.Shell.HasSessionError);
        Assert.Empty(h.Shell.Tabs);
    }

    [Fact]
    public async Task CommandFailuresAreObservedAndRunningStateIsRestored()
    {
        var gate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var failure = new InvalidOperationException("Request failed");
        var command = new AsyncRelayCommand(async _ => { await gate.Task; throw failure; });
        Exception? reported = null;
        command.ExecutionFailed += exception => reported = exception;
        command.Execute(null);
        Assert.True(command.IsRunning);
        gate.SetResult();

        await command.ExecutionTask;

        Assert.Same(failure, reported);
        Assert.False(command.IsRunning);
        Assert.True(command.CanExecute(null));
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task CommandShutdownFailuresCompleteWithoutReportingAnError(bool disposed)
    {
        var command = new AsyncRelayCommand(async _ =>
        {
            await Task.Yield();
            ObjectDisposedException.ThrowIf(disposed, typeof(AsyncRelayCommand));
            throw new OperationCanceledException();
        });
        var reported = false;
        command.ExecutionFailed += _ => reported = true;
        command.Execute(null);

        await command.ExecutionTask;

        Assert.False(reported);
        Assert.False(command.IsRunning);
    }

    [Fact]
    public async Task DisposingConnectionDuringSessionLoadCompletesWithoutFaulting()
    {
        using var h = new TestHarness();
        h.Connection.Connect(new Uri(TestHarness.ServerUrl));
        h.Connection.Tokens.SetToken(TestHarness.SessionToken);
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Handler.BeforeResponseAsync = async (_, cancellationToken) =>
        {
            started.SetResult();
            await Task.Delay(Timeout.Infinite, cancellationToken);
        };

        var initialization = h.Shell.InitializeAsync(TestContext.Current.CancellationToken);
        await started.Task.WaitAsync(TestContext.Current.CancellationToken);
        h.Connection.Dispose();

        Assert.Null(await Record.ExceptionAsync(() => initialization));
        Assert.Empty(h.Shell.Tabs);
        Assert.Null(h.Shell.SessionError);
    }

    [Fact]
    public async Task UnreachableServerDuringSessionLoadShowsAnError()
    {
        using var h = new TestHarness();
        h.Connection.Connect(new Uri(TestHarness.ServerUrl));
        h.Connection.Tokens.SetToken(TestHarness.SessionToken);
        h.Handler.SessionResponder = _ => throw new HttpRequestException("Connection reset");

        Assert.Null(await Record.ExceptionAsync(() => h.Shell.InitializeAsync(TestContext.Current.CancellationToken)));
        Assert.True(h.Shell.HasSessionError);
        Assert.Contains("could not", h.Shell.SessionError, StringComparison.OrdinalIgnoreCase);
        Assert.Empty(h.Shell.Tabs);
    }
}
