using Spotledger.Client.Realtime;
using Xunit;

namespace Spotledger.Client.Notifications.Tests;

public sealed class ToastAndStalenessTests
{
    private static Notice N(string id) => new(id, "t" + id, string.Empty, NoticeSeverity.Info, null, DateTimeOffset.UnixEpoch);

    [Fact]
    public void OneToastIsVisibleTheRestWaitAndEachDismissesAfterTheDuration()
    {
        var time = new FakeTime();
        using var queue = new ToastTray(time, TimeSpan.FromSeconds(6));
        queue.Enqueue(N("1"));
        queue.Enqueue(N("2"));
        Assert.Equal("1", queue.Current!.EventId);
        Assert.Equal(1, queue.Waiting);

        time.Advance(TimeSpan.FromSeconds(5));
        Assert.Equal("1", queue.Current!.EventId);
        time.Advance(TimeSpan.FromSeconds(1));
        Assert.Equal("2", queue.Current!.EventId);
        time.Advance(TimeSpan.FromSeconds(6));
        Assert.Null(queue.Current);
    }

    [Fact]
    public void HoverHoldsTheCountdownAndResumeKeepsTheTimeLeft()
    {
        var time = new FakeTime();
        using var queue = new ToastTray(time, TimeSpan.FromSeconds(6));
        queue.Enqueue(N("1"));
        time.Advance(TimeSpan.FromSeconds(4));
        queue.Pause();
        queue.Pause();
        time.Advance(TimeSpan.FromMinutes(5));
        Assert.NotNull(queue.Current);

        queue.Resume();
        time.Advance(TimeSpan.FromMinutes(5));
        Assert.NotNull(queue.Current);
        queue.Resume();
        time.Advance(TimeSpan.FromSeconds(1));
        Assert.NotNull(queue.Current);
        time.Advance(TimeSpan.FromSeconds(1));
        Assert.Null(queue.Current);
    }

    [Fact]
    public void DismissShowsTheNextAtOnce()
    {
        var time = new FakeTime();
        using var queue = new ToastTray(time);
        queue.Enqueue(N("1"));
        queue.Enqueue(N("2"));
        queue.Dismiss();
        Assert.Equal("2", queue.Current!.EventId);
    }

    private static (DocumentStalenessWatcher Watcher, FakeRooms Rooms) Make(Func<bool>? auto = null)
    {
        var rooms = new FakeRooms();
        return (new DocumentStalenessWatcher(rooms, auto), rooms);
    }

    [Fact]
    public void AJoinPerOpenTabAndNoneAfterTheyClose()
    {
        var (watcher, rooms) = Make();
        var one = watcher.Watch(new FakeDocument("Task", "T-1", 1));
        var two = watcher.Watch(new FakeDocument("Task", "T-2", 1));
        Assert.Equal(2, watcher.ActiveWatches);
        Assert.Equal(2, rooms.Active);
        Assert.Equal(["doc:Task:T-1", "doc:Task:T-2"], rooms.Rooms);
        one.Dispose();
        two.Dispose();
        two.Dispose();
        Assert.Equal(0, watcher.ActiveWatches);
        Assert.Equal(0, rooms.Active);
    }

    [Fact]
    public async Task ACleanFormIsOfferedAReloadAndNeverReloadedByItself()
    {
        var (watcher, rooms) = Make();
        var document = new FakeDocument("Task", "T-1", 3);
        using var watch = watcher.Watch(document);

        rooms.Deliver(watch.Room, Events.Doc(RealtimeEventKind.DocUpdated, "Task", "T-1", 4));

        Assert.Equal(StalenessKind.ChangedClean, watch.Kind);
        Assert.Equal(0, document.Reloads);
        Assert.True(await watch.ReloadAsync());
        Assert.Equal(1, document.Reloads);
        Assert.Equal(StalenessKind.None, watch.Kind);
    }

    [Fact]
    public async Task ADirtyFormNeverAutoReloadsAndReloadNeedsConfirmation()
    {
        var (watcher, rooms) = Make(() => true);
        var document = new FakeDocument("Task", "T-1", 3, dirty: true);
        using var watch = watcher.Watch(document);

        rooms.Deliver(watch.Room, Events.Doc(RealtimeEventKind.DocUpdated, "Task", "T-1", 4));

        Assert.Equal(StalenessKind.ChangedDirty, watch.Kind);
        Assert.Equal(0, document.Reloads);
        Assert.False(await watch.ReloadAsync(() => Task.FromResult(false)));
        Assert.False(await watch.ReloadAsync());
        Assert.Equal(0, document.Reloads);
        Assert.True(await watch.ReloadAsync(() => Task.FromResult(true)));
        Assert.Equal(1, document.Reloads);
    }

    [Fact]
    public void TheCleanAutoReloadSettingReloadsWhenOn()
    {
        var (watcher, rooms) = Make(() => true);
        var document = new FakeDocument("Task", "T-1", 3);
        using var watch = watcher.Watch(document);
        rooms.Deliver(watch.Room, Events.Doc(RealtimeEventKind.DocUpdated, "Task", "T-1", 4));
        Assert.Equal(1, document.Reloads);
        Assert.Equal(StalenessKind.None, watch.Kind);
    }

    [Fact]
    public void AnEventForAVersionTheFormHoldsIsIgnoredWhichCoversTheEchoOfItsOwnSave()
    {
        var (watcher, rooms) = Make();
        var document = new FakeDocument("Task", "T-1", 5);
        using var watch = watcher.Watch(document);
        rooms.Deliver(watch.Room, Events.Doc(RealtimeEventKind.DocUpdated, "Task", "T-1", 5));
        rooms.Deliver(watch.Room, Events.Doc(RealtimeEventKind.DocUpdated, "Task", "T-1", 4));
        Assert.Equal(StalenessKind.None, watch.Kind);
    }

    [Fact]
    public void AnEventForAnotherDocumentIsIgnored()
    {
        var (watcher, rooms) = Make();
        using var watch = watcher.Watch(new FakeDocument("Task", "T-1", 1));
        rooms.Deliver(watch.Room, Events.Doc(RealtimeEventKind.DocUpdated, "Task", "T-9", 9));
        Assert.Equal(StalenessKind.None, watch.Kind);
    }

    [Fact]
    public void DeleteAndCancelChangeTheBannerAndStick()
    {
        var (watcher, rooms) = Make();
        using var cancelled = watcher.Watch(new FakeDocument("Task", "T-1", 1));
        rooms.Deliver(cancelled.Room, Events.Doc(RealtimeEventKind.DocCancelled, "Task", "T-1", 2));
        Assert.Equal(StalenessKind.Cancelled, cancelled.Kind);
        rooms.Deliver(cancelled.Room, Events.Doc(RealtimeEventKind.DocUpdated, "Task", "T-1", 3));
        Assert.Equal(StalenessKind.Cancelled, cancelled.Kind);

        using var deleted = watcher.Watch(new FakeDocument("Task", "T-2", 1));
        rooms.Deliver(deleted.Room, Events.Doc(RealtimeEventKind.DocDeleted, "Task", "T-2", 2));
        Assert.Equal(StalenessKind.Deleted, deleted.Kind);
        Assert.Equal("This document was deleted by someone else.", StalenessBar.MessageFor(deleted.Kind));
    }

    [Fact]
    public void AResyncMarksTheFormMaybeStaleAndIsAcknowledged()
    {
        var (watcher, rooms) = Make();
        using var watch = watcher.Watch(new FakeDocument("Task", "T-1", 1));
        rooms.Deliver(watch.Room, new RealtimeResyncRequired(watch.Room, RealtimeResyncReason.ResumeExpired, 3));
        Assert.Equal(StalenessKind.MaybeStale, watch.Kind);
        Assert.Equal(1, rooms.Acknowledged);
    }

    [Theory]
    [InlineData(0, "Notifications")]
    [InlineData(3, "Notifications, 3 unread")]
    public void TheBellNameSaysTheUnreadCount(int unread, string name) => Assert.Equal(name, NotificationsFlyout.BellName(unread));

    [Theory]
    [InlineData(5, "now")]
    [InlineData(300, "5 min ago")]
    [InlineData(7200, "2 h ago")]
    [InlineData(200000, "2 d ago")]
    public void RelativeTimeComesFromTheInjectedClock(int seconds, string expected)
    {
        var now = new DateTimeOffset(2026, 10, 30, 9, 0, 0, TimeSpan.Zero);
        Assert.Equal(expected, NoticeLook.Relative(now - TimeSpan.FromSeconds(seconds), now));
    }
}
