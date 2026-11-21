using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Desktop.Hosting;
using Spotledger.Client.Forms;
using Spotledger.Client.Notifications;
using Spotledger.Client.Notifications.Tests;
using Spotledger.Client.Realtime;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.TestSupport.A11y;
using Xunit;

namespace Spotledger.Client.Shell.Tests.Wiring;

/// <summary>
/// The notification centre wired into the shell (WP-UI-02.T6): the <c>view.notifications</c> command, the
/// bell, the flyout and the toast hosted by the shell view, link activation, the room lifecycle and the
/// per-user state, all over the notification fakes (a fake room source, a manual clock).
/// </summary>
public sealed class NotificationWiringTests
{
    private sealed class FakeSession(FakeRooms rooms) : IRoomSession
    {
        public IRoomSource Rooms { get; } = rooms;

        public bool Disposed { get; private set; }

        public ValueTask DisposeAsync()
        {
            Disposed = true;
            return ValueTask.CompletedTask;
        }
    }

    private sealed class Kit : IDisposable
    {
        public Kit(Rig rig)
        {
            Rig = rig;
            Rooms = new FakeRooms();
            Session = new FakeSession(Rooms);
            Store = new InMemoryNoticeStateStore();
            Host = new NotificationHost(
                rig.Shell,
                () => Session,
                () => TestHarness.ServerUrl,
                (doctype, id) => Opened.Add((doctype, id)),
                Store,
                new FakeTime(),
                post: action => action());
        }

        public Rig Rig { get; }

        public FakeRooms Rooms { get; }

        public FakeSession Session { get; }

        public InMemoryNoticeStateStore Store { get; }

        public NotificationHost Host { get; }

        public List<(string DocType, string Id)> Opened { get; } = [];

        public string UserRoom => "user:" + Rig.Shell.UserId;

        public void Dispose() => Host.Dispose();
    }

    private static void Pump()
    {
        for (var round = 0; round < 3; round++)
        {
            Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
        }
    }

    private static async Task<Kit> SignedInAsync()
    {
        var rig = new Rig();
        await rig.SignInAsync();
        return new Kit(rig);
    }

    [AvaloniaFact]
    public async Task TheCommandIsRegisteredBoundOnEveryProfileAndFoundByTheLauncher()
    {
        using var kit = await SignedInAsync();
        var shell = kit.Rig.Shell;

        Assert.Equal("Notifications", shell.Registry[ShellViewModel.NotificationsCommandId].Label);
        Assert.True(shell.Commands.ContainsKey(ShellViewModel.NotificationsCommandId));
        Assert.Contains(shell.Registry.MenuItems("View"), item => item.CommandId == ShellViewModel.NotificationsCommandId);
        foreach (var profile in new[] { "desktop", "browser" })
        {
            var keymap = KeymapProfile.Load(profile);
            keymap.Validate(shell.Registry);
            Assert.Contains(keymap.Bindings, binding => binding.CommandId == ShellViewModel.NotificationsCommandId);
        }

        Assert.Contains(shell.Registry.Definitions, d => d.Id == ShellViewModel.DoNotDisturbCommandId);
        Assert.Contains(shell.Registry.Definitions, d => d.Id == ShellViewModel.AutoReloadCommandId);
    }

    [AvaloniaFact]
    public async Task ExecutingTheCommandTogglesTheFlyoutInTheShellView()
    {
        using var kit = await SignedInAsync();
        var shell = kit.Rig.Shell;
        var view = new ShellView { DataContext = shell };
        var window = new Window { Width = 1280, Height = 800, Content = view };
        window.Show();
        Pump();

        Assert.False(shell.IsNotificationsOpen);
        await shell.RunAsync(ShellViewModel.NotificationsCommandId, CommandRoute.Shortcut);
        Pump();
        Assert.True(shell.IsNotificationsOpen);
        Assert.True(view.NotificationsFlyoutControl?.IsOpen);
        Assert.Same(kit.Host.Surface!.Flyout, view.NotificationsFlyoutControl!.Content);

        await shell.RunAsync(ShellViewModel.NotificationsCommandId, CommandRoute.Shortcut);
        Pump();
        Assert.False(shell.IsNotificationsOpen);
        Assert.False(view.NotificationsFlyoutControl!.IsOpen);
        window.Close();
    }

    [AvaloniaFact]
    public async Task TheBellShowsTheFeedsUnreadCountAndItsAccessibleName()
    {
        using var kit = await SignedInAsync();
        var shell = kit.Rig.Shell;
        var view = new ShellView { DataContext = shell };
        var window = new Window { Width = 1280, Height = 800, Content = view };
        window.Show();
        Pump();
        var bell = view.FindControl<Button>("BellButton")!;
        Assert.Equal("Notifications", AutomationProperties.GetName(bell));

        kit.Rooms.Deliver(kit.UserRoom, Events.Notice("e1", title: "First"));
        kit.Rooms.Deliver(kit.UserRoom, Events.Notice("e2", title: "Second"));
        Pump();

        Assert.Equal(2, shell.NotificationCount);
        Assert.Equal("Notifications, 2 unread", shell.NotificationBellName);
        Assert.Equal("Notifications, 2 unread", AutomationProperties.GetName(bell));
        Assert.Equal("2", view.FindControl<TextBlock>("BellCount")!.Text);

        kit.Host.Surface!.Feed.MarkAllRead();
        Pump();
        Assert.Equal(0, shell.NotificationCount);
        Assert.Equal("Notifications", AutomationProperties.GetName(bell));
        window.Close();
    }

    [AvaloniaFact]
    public async Task AToastAppearsForANewNoticeAndNotUnderDoNotDisturb()
    {
        using var kit = await SignedInAsync();
        var shell = kit.Rig.Shell;
        var view = new ShellView { DataContext = shell };
        var window = new Window { Width = 1280, Height = 800, Content = view };
        window.Show();
        Pump();
        var tray = kit.Host.Surface!.Toasts;

        kit.Rooms.Deliver(kit.UserRoom, Events.Notice("e1", title: "Invoice paid"));
        Pump();
        Assert.Equal("e1", tray.Current?.EventId);
        Assert.Same(kit.Host.Surface.Toast, view.ToastHostControl.Content);
        var toast = Assert.IsType<ToastView>(view.ToastHostControl.Content);
        Assert.True(toast.IsVisible);
        Assert.False(toast.Focusable, "a toast never takes keyboard focus");
        Assert.Equal(AutomationLiveSetting.Polite, AutomationProperties.GetLiveSetting(toast));

        tray.Dismiss();
        await shell.RunAsync(ShellViewModel.DoNotDisturbCommandId, CommandRoute.Menu);
        Assert.True(kit.Host.Surface.DoNotDisturb);
        kit.Rooms.Deliver(kit.UserRoom, Events.Notice("e2", title: "Quiet"));
        Pump();
        Assert.Null(tray.Current);
        Assert.Equal(2, shell.NotificationCount);

        await shell.RunAsync(ShellViewModel.DoNotDisturbCommandId, CommandRoute.Menu);
        Assert.False(kit.Host.Surface.DoNotDisturb);
        window.Close();
    }

    [AvaloniaFact]
    public async Task ActivatingANoticesLinkNavigatesThroughTheShellAndClosesTheFlyout()
    {
        using var kit = await SignedInAsync();
        var shell = kit.Rig.Shell;
        kit.Rooms.Deliver(kit.UserRoom, Events.Notice("e1", title: "Task due", doctype: "Task", id: "T-1"));
        shell.IsNotificationsOpen = true;
        var flyout = Assert.IsType<NotificationsFlyout>(kit.Host.Surface!.Flyout);
        var window = A11yHost.Show(flyout, 420, 480);
        try
        {
            var row = flyout.GetVisualDescendants().OfType<Button>().First(b => b.Content is not string);
            row.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));

            Assert.Equal([("Task", "T-1")], kit.Opened);
            Assert.False(shell.IsNotificationsOpen);
            Assert.Equal(0, shell.NotificationCount);
        }
        finally
        {
            A11yHost.Close(window);
        }
    }

    [AvaloniaFact]
    public async Task TheFeedJoinsTheUserRoomWhenTheSessionStartsAndLeavesOnSignOut()
    {
        using var kit = await SignedInAsync();
        var shell = kit.Rig.Shell;

        Assert.Equal([kit.UserRoom], kit.Rooms.Rooms);
        Assert.NotNull(shell.Notifications);

        shell.HandleSessionExpired();
        Pump();

        Assert.Equal(0, kit.Rooms.Active);
        Assert.Null(shell.Notifications);
        Assert.Null(kit.Host.Surface);
        Assert.Equal(0, shell.NotificationCount);
        Assert.False(shell.IsNotificationsOpen);
        Assert.True(kit.Session.Disposed, "the realtime connection of the session is closed");
    }

    [AvaloniaFact]
    public async Task ReadStateIsKeptPerServerAndUser()
    {
        using var kit = await SignedInAsync();
        kit.Rooms.Deliver(kit.UserRoom, Events.Notice("e1", title: "Read me"));
        kit.Host.Surface!.Feed.MarkRead("e1");

        Assert.Equal(["e1"], kit.Store.Load(TestHarness.ServerUrl, kit.Rig.Shell.UserId).ReadIds);
        Assert.Empty(kit.Store.Load(TestHarness.ServerUrl, "someone-else").ReadIds);
        Assert.Empty(kit.Store.Load("http://other.test", kit.Rig.Shell.UserId).ReadIds);
    }

    [AvaloniaFact]
    public async Task WithoutANotificationCentreTheCommandOnlySaysSo()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var shell = rig.Shell;

        await shell.RunAsync(ShellViewModel.NotificationsCommandId, CommandRoute.Menu);

        Assert.False(shell.IsNotificationsOpen);
        Assert.Equal("Notifications are not connected.", shell.StatusMessage);
    }

    [AvaloniaFact]
    public async Task EachOpenFormTabJoinsItsDocumentRoomOnceAndLeavesWhenTheTabCloses()
    {
        using var kit = await SignedInAsync();
        kit.Rig.Host.StalenessFactory = kit.Host.WatchForm;

        var form = await kit.Rig.OpenAsync("INV-1");
        _ = await kit.Rig.OpenAsync("INV-1");
        Assert.Equal(1, kit.Host.ActiveWatches);
        Assert.Equal(2, kit.Rooms.Active);
        Assert.Contains("doc:invoice:INV-1", kit.Rooms.Rooms);

        kit.Rooms.Deliver("doc:invoice:INV-1", Events.Doc(RealtimeEventKind.DocUpdated, "invoice", "INV-1", 4));
        Pump();
        var bar = Assert.IsType<StalenessBar>(form.Banner);
        Assert.True(bar.IsVisible);

        kit.Rig.Shell.CloseTab(kit.Rig.Shell.SelectedTab!);
        await TestHarness.WaitUntilAsync(() => kit.Host.ActiveWatches == 0, "the watch to leave with its tab");
        Assert.Equal(0, kit.Host.ActiveWatches);
        Assert.Equal(1, kit.Rooms.Active);
    }
}
