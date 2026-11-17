using Avalonia.Threading;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Notifications;

/// <summary>
/// The notification centre as the shell hosts it (WP-UI-02.T6): the feed, the flyout under the bell and
/// the toast, joined into the one <see cref="INotificationSurface"/> the shell attaches. A new notice
/// raises a toast unless do-not-disturb is on or the app is not focused (the feed decides); opening a
/// notice's link goes to <c>openDocument</c> (the shell's open-document path, never a URL) and then asks
/// the flyout to close. Disposing it leaves the user room, so sign-out leaves.
/// </summary>
public sealed class NotificationSurface : INotificationSurface, IDisposable
{
    private readonly NotificationFeed _feed;
    private readonly ToastTray _tray;
    private readonly NotificationsFlyout _flyout;
    private readonly ToastView _toast;
    private readonly Action<Action> _post;
    private bool _disposed;

    /// <summary>Creates the surface over a feed. Controls are built here: call it on the UI thread.</summary>
    /// <param name="feed">The user's notices; the surface owns it and disposes it.</param>
    /// <param name="time">The clock for toasts and relative times.</param>
    /// <param name="openDocument">Opens a document (doctype, id) through the shell's open-document path.</param>
    /// <param name="post">Runs an action on the UI thread; the Avalonia dispatcher when absent.</param>
    /// <param name="toastDuration">How long a toast stays; the toast token when absent.</param>
    public NotificationSurface(
        NotificationFeed feed,
        TimeProvider time,
        Action<string, string> openDocument,
        Action<Action>? post = null,
        TimeSpan? toastDuration = null)
    {
        _feed = feed ?? throw new ArgumentNullException(nameof(feed));
        ArgumentNullException.ThrowIfNull(time);
        ArgumentNullException.ThrowIfNull(openDocument);
        _post = post ?? (action => Dispatcher.UIThread.Post(action));
        _tray = new ToastTray(time, toastDuration);
        _flyout = new NotificationsFlyout(feed, time, (doctype, id) =>
        {
            openDocument(doctype, id);
            CloseRequested?.Invoke(this, EventArgs.Empty);
        });
        _toast = new ToastView(_tray);
        _feed.Changed += OnFeedChanged;
        _feed.ToastRequested += OnToastRequested;
    }

    /// <summary>The notices behind the bell.</summary>
    public NotificationFeed Feed => _feed;

    /// <summary>The toast queue behind the toast view.</summary>
    public ToastTray Toasts => _tray;

    /// <inheritdoc/>
    public int UnreadCount => _feed.UnreadCount;

    /// <inheritdoc/>
    public string BellName => NotificationsFlyout.BellName(_feed.UnreadCount);

    /// <inheritdoc/>
    public object? Flyout => _flyout;

    /// <inheritdoc/>
    public object? Toast => _toast;

    /// <inheritdoc/>
    public bool DoNotDisturb
    {
        get => _feed.DoNotDisturb;
        set => _feed.DoNotDisturb = value;
    }

    /// <inheritdoc/>
    public bool AutoReloadClean
    {
        get => _feed.AutoReloadClean;
        set => _feed.AutoReloadClean = value;
    }

    /// <inheritdoc/>
    public event EventHandler? Changed;

    /// <inheritdoc/>
    public event EventHandler? CloseRequested;

    /// <summary>Joins the user's room (the session started). Calling it twice has no effect.</summary>
    public void Start() => _feed.Start();

    /// <summary>Leaves the user's room and stops the toasts (sign-out).</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _feed.Changed -= OnFeedChanged;
        _feed.ToastRequested -= OnToastRequested;
        _toast.Dispose();
        _flyout.Dispose();
        _tray.Dispose();
        _feed.Dispose();
    }

    private void OnFeedChanged(object? sender, EventArgs e) => _post(() =>
    {
        if (!_disposed)
        {
            Changed?.Invoke(this, EventArgs.Empty);
        }
    });

    private void OnToastRequested(object? sender, Notice notice)
    {
        if (!_disposed)
        {
            _tray.Enqueue(notice);
        }
    }
}
