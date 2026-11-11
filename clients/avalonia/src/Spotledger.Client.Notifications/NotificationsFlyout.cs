using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Notifications;

/// <summary>
/// The bell's flyout: the notices newest first, each with a severity shape, a relative time and (for a
/// notice with a link) activation that opens the document through <c>openDocument</c>, then "Mark all read".
/// All notice text is plain text. The bell button exposes "Notifications, N unread" as its accessible name.
/// </summary>
public sealed class NotificationsFlyout : UserControl, IDisposable
{
    private readonly NotificationFeed _feed;
    private readonly TimeProvider _time;
    private readonly Action<string, string> _openDocument;
    private readonly StackPanel _list = new() { Spacing = 4 };
    private readonly TextBlock _empty = NoticeLook.Text("No notifications while you were connected.", 3);
    private readonly Button _markAll = new() { Content = "Mark all read" };
    private readonly List<IDisposable> _subscriptions = [];

    /// <summary>Creates the flyout content.</summary>
    /// <param name="feed">The notices.</param>
    /// <param name="time">The clock for relative times.</param>
    /// <param name="openDocument">Opens a document (doctype, id) with the shell's open-document command.</param>
    public NotificationsFlyout(NotificationFeed feed, TimeProvider time, Action<string, string> openDocument)
    {
        _feed = feed ?? throw new ArgumentNullException(nameof(feed));
        _time = time ?? throw new ArgumentNullException(nameof(time));
        _openDocument = openDocument ?? throw new ArgumentNullException(nameof(openDocument));
        MinWidth = 320;
        MaxWidth = 420;
        MaxHeight = 480;
        AutomationProperties.SetName(this, "Notifications");
        _markAll.Click += (_, _) => _feed.MarkAllRead();
        var header = new DockPanel { Margin = new Thickness(0, 0, 0, 8) };
        var title = NoticeLook.Text("Notifications", 1, 16);
        title.FontWeight = FontWeight.SemiBold;
        DockPanel.SetDock(_markAll, Dock.Right);
        header.Children.Add(_markAll);
        header.Children.Add(title);
        var root = new DockPanel { Margin = new Thickness(12) };
        DockPanel.SetDock(header, Dock.Top);
        root.Children.Add(header);
        root.Children.Add(new ScrollViewer { Content = new StackPanel { Children = { _empty, _list } } });
        Content = root;
        _subscriptions.Add(NoticeLook.BindBrush(this, Tokens.TokenColorTextPrimary, brush => Foreground = brush));
        _subscriptions.Add(NoticeLook.BindBrush(this, Tokens.TokenColorSurfaceRaised, brush => Background = brush));
        _feed.Changed += OnChanged;
        Rebuild();
    }

    /// <summary>The accessible name of the bell: "Notifications, 3 unread" (or just "Notifications").</summary>
    public static string BellName(int unread) => unread > 0 ? $"Notifications, {unread} unread" : "Notifications";

    /// <inheritdoc/>
    public void Dispose()
    {
        _feed.Changed -= OnChanged;
        foreach (var subscription in _subscriptions)
        {
            subscription.Dispose();
        }
    }

    private void OnChanged(object? sender, EventArgs e) => Dispatcher.UIThread.Post(Rebuild);

    private void Rebuild()
    {
        var notices = _feed.Notices;
        _empty.IsVisible = notices.Count == 0;
        _markAll.IsEnabled = _feed.UnreadCount > 0;
        _list.Children.Clear();
        var now = _time.GetUtcNow();
        foreach (var notice in notices)
        {
            _list.Children.Add(Row(notice, now));
        }
    }

    private Button Row(Notice notice, DateTimeOffset now)
    {
        var read = _feed.IsRead(notice.EventId);
        var marker = new Avalonia.Controls.Shapes.Path
        {
            Data = Geometry.Parse(NoticeLook.Shape(notice.Severity)),
            StrokeThickness = 2,
            Width = 20,
            Height = 20,
            Stretch = Stretch.Uniform,
            VerticalAlignment = VerticalAlignment.Top,
            Margin = new Thickness(0, 2, 8, 0),
        };
        _subscriptions.Add(NoticeLook.BindBrush(marker, NoticeLook.ColourToken(notice.Severity), brush => marker.Stroke = brush));
        var title = NoticeLook.Text(notice.Title, 2);
        title.FontWeight = read ? FontWeight.Normal : FontWeight.SemiBold;
        var text = new StackPanel { Spacing = 2, Children = { title } };
        if (notice.Body.Length > 0)
        {
            text.Children.Add(NoticeLook.Text(notice.Body, 3, 13));
        }

        text.Children.Add(NoticeLook.Text(NoticeLook.Relative(notice.ReceivedAt, now), 1, 12));
        var row = new Button
        {
            HorizontalAlignment = HorizontalAlignment.Stretch,
            HorizontalContentAlignment = HorizontalAlignment.Left,
            Content = new DockPanel { Children = { marker, text } },
        };
        DockPanel.SetDock(marker, Dock.Left);
        AutomationProperties.SetName(row, $"{NoticeLook.Name(notice.Severity)}: {notice.Title}{(read ? string.Empty : ", unread")}");
        row.Click += (_, _) =>
        {
            _feed.MarkRead(notice.EventId);
            if (notice.Link is { } link)
            {
                // The shell's open fails with the normal permission message if the user cannot read it.
                _openDocument(link.DocType, link.Id);
            }
        };
        return row;
    }
}
