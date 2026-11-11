using Avalonia.Controls;
using Avalonia.VisualTree;
using Avalonia.Headless.XUnit;
using Spotledger.Client.TestSupport.A11y;
using Xunit;

namespace Spotledger.Client.Notifications.Tests;

public sealed class ViewTests
{
    [AvaloniaFact]
    public void TheFlyoutShowsHostileTextAsBoundedPlainTextAndOpensALink()
    {
        var rooms = new FakeRooms();
        var feed = new NotificationFeed(rooms, new InMemoryNoticeStateStore(), "s", "u1");
        feed.Start();
        rooms.Deliver("user:u1", Events.Notice("e1", title: "<b>bold</b>" + new string('x', 5000), doctype: "Task", id: "T-1"));
        var opened = new List<(string, string)>();
        var flyout = new NotificationsFlyout(feed, new FakeTime(), (doctype, id) => opened.Add((doctype, id)));
        var window = A11yHost.Show(flyout, 420, 480);
        try
        {
            var texts = flyout.GetVisualDescendants().OfType<TextBlock>().Select(t => t.Text ?? string.Empty).ToList();
            Assert.Contains(texts, t => t.StartsWith("<b>bold</b>", StringComparison.Ordinal));
            Assert.All(texts, t => Assert.True(t.Length <= NoticeParser.MaxBody + 1));

            var row = flyout.GetVisualDescendants().OfType<Button>().First(b => b.Content is not string);
            row.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));
            Assert.Equal([("Task", "T-1")], opened);
            Assert.Equal(0, feed.UnreadCount);
        }
        finally
        {
            A11yHost.Close(window);
            flyout.Dispose();
        }
    }

    [AvaloniaFact]
    public void TheToastNeverTakesFocusAndIsAPoliteLiveRegion()
    {
        using var queue = new ToastTray(new FakeTime());
        var view = new ToastView(queue);
        Assert.False(view.Focusable);
        Assert.Equal(Avalonia.Automation.AutomationLiveSetting.Polite, Avalonia.Automation.AutomationProperties.GetLiveSetting(view));
        queue.Enqueue(new Notice("1", "Hello", string.Empty, NoticeSeverity.Warning, null, DateTimeOffset.UnixEpoch));
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
        Assert.True(view.IsVisible);
        Assert.Equal("Warning: Hello", Avalonia.Automation.AutomationProperties.GetName(view));
        view.Dispose();
    }
}
