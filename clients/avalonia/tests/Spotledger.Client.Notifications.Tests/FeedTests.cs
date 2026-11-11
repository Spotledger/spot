using System.Text;
using Xunit;

namespace Spotledger.Client.Notifications.Tests;

public sealed class FeedTests
{
    private static (NotificationFeed Feed, FakeRooms Rooms, InMemoryNoticeStateStore Store) Make(bool focused = true, InMemoryNoticeStateStore? store = null)
    {
        var rooms = new FakeRooms();
        store ??= new InMemoryNoticeStateStore();
        var feed = new NotificationFeed(rooms, store, "https://erp.example.test", "u1", () => focused);
        feed.Start();
        return (feed, rooms, store);
    }

    [Fact]
    public void ItJoinsTheUsersRoomOnceAndShowsNewestFirst()
    {
        var (feed, rooms, _) = Make();
        feed.Start();
        Assert.Equal(["user:u1"], rooms.Rooms);

        rooms.Deliver("user:u1", Events.Notice("e1", title: "first"));
        rooms.Deliver("user:u1", Events.Notice("e2", title: "second"));

        Assert.Equal(["second", "first"], feed.Notices.Select(n => n.Title));
        Assert.Equal(2, feed.UnreadCount);
    }

    [Fact]
    public void ARedeliveredNoticeIsIgnored()
    {
        var (feed, rooms, _) = Make();
        rooms.Deliver("user:u1", Events.Notice("e1"));
        rooms.Deliver("user:u1", Events.Notice("e1"));
        Assert.Single(feed.Notices);
    }

    [Fact]
    public void OnlyTheLast200AreKept()
    {
        var (feed, rooms, _) = Make();
        for (var i = 0; i < 250; i++) rooms.Deliver("user:u1", Events.Notice("e" + i, title: "n" + i));
        Assert.Equal(200, feed.Notices.Count);
        Assert.Equal("n249", feed.Notices[0].Title);
        Assert.Equal("n50", feed.Notices[^1].Title);
        Assert.Equal(200, feed.UnreadCount);
    }

    [Fact]
    public void MarkReadAndMarkAllReadLowerTheCount()
    {
        var (feed, rooms, _) = Make();
        for (var i = 0; i < 3; i++) rooms.Deliver("user:u1", Events.Notice("e" + i));
        feed.MarkRead("e1");
        Assert.Equal(2, feed.UnreadCount);
        feed.MarkAllRead();
        Assert.Equal(0, feed.UnreadCount);
        Assert.Equal(3, feed.Notices.Count);
    }

    [Fact]
    public void ReadStateSurvivesARestartAndAReplayedNoticeStaysRead()
    {
        var (feed, rooms, store) = Make();
        rooms.Deliver("user:u1", Events.Notice("e1"));
        rooms.Deliver("user:u1", Events.Notice("e2"));
        feed.MarkRead("e1");
        feed.Dispose();

        var (restarted, rooms2, _) = Make(store: store);
        Assert.Empty(restarted.Notices);
        Assert.Equal(0, restarted.UnreadCount);
        rooms2.Deliver("user:u1", Events.Notice("e1"));
        rooms2.Deliver("user:u1", Events.Notice("e2"));
        Assert.Equal(1, restarted.UnreadCount);
        Assert.True(restarted.IsRead("e1"));
    }

    [Fact]
    public void TheStoredStateHoldsIdsAndFlagsOnlyAndIsPrunedToTheWindow()
    {
        var (feed, rooms, store) = Make();
        for (var i = 0; i < 260; i++) rooms.Deliver("user:u1", Events.Notice("e" + i, title: "SECRET-TITLE", body: "SECRET-BODY"));
        feed.MarkAllRead();
        var state = store.Load("https://erp.example.test", "u1");
        Assert.Equal(200, state.ReadIds.Count);
        Assert.DoesNotContain(state.ReadIds, id => id.Contains("SECRET", StringComparison.Ordinal));
    }

    [Fact]
    public void TheFileStoreRoundTripsAndKeepsNoBody()
    {
        var root = Path.Combine(Path.GetTempPath(), "notices-" + Guid.NewGuid().ToString("N"));
        try
        {
            var store = new FileNoticeStateStore(root);
            store.Save("s", "u", new NoticeState(["a", "b"], true, true));
            var again = new FileNoticeStateStore(root).Load("s", "u");
            Assert.Equal(["a", "b"], again.ReadIds);
            Assert.True(again.DoNotDisturb);
            Assert.True(again.AutoReloadClean);
            Assert.Equal(NoticeState.Empty.ReadIds, new FileNoticeStateStore(root).Load("s", "other").ReadIds);
            foreach (var file in Directory.GetFiles(root)) File.WriteAllText(file, "{ not json");
            Assert.Empty(new FileNoticeStateStore(root).Load("s", "u").ReadIds);
        }
        finally
        {
            if (Directory.Exists(root)) Directory.Delete(root, recursive: true);
        }
    }

    [Fact]
    public void AToastIsRequestedOnlyWhenFocusedAndNotDisturbed()
    {
        var (feed, rooms, _) = Make();
        var toasts = new List<Notice>();
        feed.ToastRequested += (_, notice) => toasts.Add(notice);
        rooms.Deliver("user:u1", Events.Notice("e1"));
        Assert.Single(toasts);

        feed.DoNotDisturb = true;
        rooms.Deliver("user:u1", Events.Notice("e2"));
        Assert.Single(toasts);
        Assert.Equal(2, feed.UnreadCount);

        var (unfocused, rooms2, _) = Make(focused: false);
        unfocused.ToastRequested += (_, notice) => toasts.Add(notice);
        rooms2.Deliver("user:u1", Events.Notice("e3"));
        Assert.Single(toasts);
        Assert.Equal(1, unfocused.UnreadCount);
    }

    [Fact]
    public void DoNotDisturbIsRememberedLocally()
    {
        var (feed, _, store) = Make();
        feed.DoNotDisturb = true;
        Assert.True(store.Load("https://erp.example.test", "u1").DoNotDisturb);
    }

    [Fact]
    public void AResyncInventsNothingAndIsAcknowledged()
    {
        var (feed, rooms, _) = Make();
        rooms.Deliver("user:u1", Events.Notice("e1"));
        rooms.Deliver("user:u1", new Realtime.RealtimeResyncRequired("user:u1", Realtime.RealtimeResyncReason.ResumeExpired, 5));
        Assert.Single(feed.Notices);
        Assert.Equal(1, feed.ResyncCount);
    }

    [Fact]
    public void UnknownEventsAndKindsAreIgnored()
    {
        var (feed, rooms, _) = Make();
        var doc = Events.Doc(Realtime.RealtimeEventKind.DocUpdated, "Task", "T-1", 2);
        rooms.Deliver("user:u1", doc);
        var other = System.Text.Json.JsonSerializer.SerializeToElement(new { event_id = "x", @event = "identity_user_created" });
        rooms.Deliver("user:u1", doc with { Kind = Realtime.RealtimeEventKind.Notification, Payload = other });
        Assert.Empty(feed.Notices);
    }

    [Fact]
    public void HostileTextBecomesBoundedPlainText()
    {
        var huge = new string('A', 100 * 1024);
        var hostile = "<script>alert(1)</script>‮ evil\u0000\u0007\n\tline" + huge;
        Assert.True(NoticeParser.TryParse(Events.Notice("e1", title: hostile, body: hostile, severity: "error"), out var notice));
        Assert.True(notice.Title.Length <= NoticeParser.MaxTitle + 1);
        Assert.True(notice.Body.Length <= NoticeParser.MaxBody + 1);
        Assert.DoesNotContain(notice.Title, c => char.IsControl(c) || c == '‮');
        Assert.StartsWith("<script>alert(1)</script> evil line", notice.Title, StringComparison.Ordinal);
        Assert.EndsWith("…", notice.Body, StringComparison.Ordinal);
        Assert.Equal(NoticeSeverity.Error, notice.Severity);
        Assert.True(Encoding.UTF8.GetByteCount(notice.Body) < 2048);
    }

    [Theory]
    [InlineData("Sales Invoice", "SINV-0001", true)]
    [InlineData("javascript:alert(1)", "x", false)]
    [InlineData("Sales Invoice", "https://evil.example/x?y=<a>", false)]
    [InlineData("Task", "../../etc/passwd", false)]
    [InlineData("", "x", false)]
    public void ALinkIsADocumentReferenceNeverAUrl(string doctype, string id, bool valid)
    {
        Assert.True(NoticeParser.TryParse(Events.Notice("e1", doctype: doctype, id: id), out var notice));
        Assert.Equal(valid, notice.Link is not null);
        if (valid) Assert.Equal(new NoticeLink(doctype, id), notice.Link);
    }

    [Fact]
    public void ACutNeverSplitsASurrogatePair()
    {
        var text = new string('a', 119) + "\U0001F600" + "tail";
        var clean = NoticeParser.Clean(text, 120);
        Assert.DoesNotContain(clean, char.IsSurrogate);
    }
}
