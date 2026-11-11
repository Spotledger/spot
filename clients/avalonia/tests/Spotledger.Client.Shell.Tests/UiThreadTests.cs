using System.Collections.Concurrent;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

public sealed class UiThreadTests
{
    [AvaloniaFact]
    public async Task AsyncSessionLifecycleKeepsBoundStateOnUiThread()
    {
        using var h = new TestHarness();
        var notifications = new ConcurrentQueue<(string Name, bool OnUiThread)>();
        void Record(string name) => notifications.Enqueue((name, Dispatcher.UIThread.CheckAccess()));

        h.Login.PropertyChanged += (_, e) => Record($"Login.{e.PropertyName}");
        h.Shell.PropertyChanged += (_, e) => Record($"Shell.{e.PropertyName}");
        h.App.PropertyChanged += (_, e) => Record($"App.{e.PropertyName}");
        h.Picker.PropertyChanged += (_, e) => Record($"Picker.{e.PropertyName}");
        h.Picker.RecentServers.CollectionChanged += (_, _) => Record("RecentServers");
        h.Shell.Tabs.CollectionChanged += (_, _) => Record("Tabs");
        h.Login.SignInCommand.CanExecuteChanged += (_, _) => Record("SignInCommand");
        h.Shell.LogoutCommand.CanExecuteChanged += (_, _) => Record("LogoutCommand");

        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();
        await TestHarness.WaitUntilAsync(() => !h.Login.SignInCommand.IsRunning, "sign-in command to finish");
        Assert.Equal("Ayesha Khan", h.Shell.UserName);
        Assert.Single(h.Shell.Tabs);

        h.Shell.LogoutCommand.Execute(null);
        await TestHarness.WaitUntilAsync(() => !h.Shell.LogoutCommand.IsRunning, "logout command to finish");
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
        Assert.Empty(h.Shell.Tabs);

        await h.SignInAsync();
        await h.ShellInitializedAsync();
        h.Handler.SessionExpiredCode = "auth.session_expired";
        await h.Shell.InitializeAsync(TestContext.Current.CancellationToken);
        await TestHarness.WaitUntilAsync(() => h.App.ShowLogin, "session expiry to return to login");
        Assert.Empty(h.Shell.Tabs);
        Assert.False(h.Connection.Tokens.HasToken);

        // Record inside the event callbacks and assert here so application error handling
        // cannot swallow a failed assertion and turn it into a form message.
        Assert.Contains(notifications, n => n.Name == "Login.Password");
        Assert.Contains(notifications, n => n.Name == "Shell.UserName");
        Assert.Contains(notifications, n => n.Name == "Tabs");
        Assert.Contains(notifications, n => n.Name == "RecentServers");
        Assert.All(notifications, n => Assert.True(n.OnUiThread, $"{n.Name} changed off the UI thread"));
    }
}
