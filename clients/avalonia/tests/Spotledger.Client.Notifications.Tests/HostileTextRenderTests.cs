using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.VisualTree;
using Spotledger.Client.TestSupport.A11y;
using Xunit;

namespace Spotledger.Client.Notifications.Tests;

/// <summary>
/// WP-TST-14.T15: a notice's text is bounded as a string (<see cref="NoticeParser"/>) and the flyout
/// must bound it as a drawing too. Hostile titles and bodies render a frame (A11yHost.Show settles
/// one, within the capture deadline) and a row never measures larger than the stated bound.
/// No screenshot baseline: the assertion is the number.
/// </summary>
public sealed class HostileTextRenderTests
{
    /// <summary>A row holds at most 2 title lines, 3 body lines and a time line (about 150 px) plus padding: 240 is generous and still a bound.</summary>
    private const double MaxRowHeight = 240;

    public static TheoryData<string, string> Hostile() => new()
    {
        { "unbroken", new string('x', 5000) },
        { "spaces", new string(' ', 5000) + "end" },
        { "right-to-left", string.Concat(Enumerable.Repeat("אבגד ", 400)) },
        { "combining", "e" + string.Concat(Enumerable.Repeat("́", 4000)) },
    };

    [AvaloniaTheory]
    [MemberData(nameof(Hostile))]
    public void AHostileTitleAndBodyRenderAFrameAndABoundedRow(string kind, string hostile)
    {
        Assert.NotEmpty(kind);
        var rooms = new FakeRooms();
        var feed = new NotificationFeed(rooms, new InMemoryNoticeStateStore(), "s", "u1");
        feed.Start();
        rooms.Deliver("user:u1", Events.Notice("e1", title: hostile, body: hostile, doctype: "Task", id: "T-1"));
        var flyout = new NotificationsFlyout(feed, new FakeTime(), (_, _) => { });
        var window = A11yHost.Show(flyout, 420, 480);
        try
        {
            var rows = flyout.GetVisualDescendants().OfType<Button>().Where(b => b.Content is not string).ToList();
            Assert.NotEmpty(rows);
            Assert.All(rows, row =>
            {
                Assert.True(row.Bounds.Width <= NotificationsFlyout.RowMaxWidth + 0.5, $"{kind}: row width {row.Bounds.Width}");
                Assert.True(row.Bounds.Height <= MaxRowHeight, $"{kind}: row height {row.Bounds.Height}");
            });
        }
        finally
        {
            A11yHost.Close(window);
            flyout.Dispose();
        }
    }
}
