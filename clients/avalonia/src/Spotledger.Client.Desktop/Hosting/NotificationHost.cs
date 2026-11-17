using System.ComponentModel;
using Avalonia.Threading;
using Spotledger.Client.Forms;
using Spotledger.Client.Notifications;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Desktop.Hosting;

/// <summary>
/// Connects the notification centre to the desktop shell (WP-UI-02.T6): when a session starts it opens
/// the realtime rooms, builds the user's feed (read state kept per server and user) and attaches it to the
/// shell, so the bell shows its unread count, the flyout and the toast are hosted by the shell view and
/// <c>view.notifications</c> works; when the session ends it leaves the user room and detaches. It also
/// gives each open form its staleness watch (<see cref="WatchForm"/>), one join per tab. It adds no rule of
/// its own: the feed, the toast queue and the watcher are the Notifications types.
/// </summary>
internal sealed class NotificationHost : IDisposable
{
    private readonly ShellViewModel _shell;
    private readonly Func<IRoomSession> _sessionFactory;
    private readonly Func<string> _serverId;
    private readonly Action<string, string> _openDocument;
    private readonly INoticeStateStore _store;
    private readonly TimeProvider _clock;
    private readonly Func<bool> _appIsFocused;
    private readonly Action<Action> _post;
    private IRoomSession? _session;
    private NotificationSurface? _surface;
    private DocumentStalenessWatcher? _watcher;
    private string _user = string.Empty;
    private bool _disposed;

    /// <summary>Creates the host and listens to the shell's session.</summary>
    /// <param name="shell">The shell the bell and the flyout belong to.</param>
    /// <param name="sessionFactory">Opens the rooms of a session (the realtime client in the app).</param>
    /// <param name="serverId">The server the person is signed in to (part of the read state's key).</param>
    /// <param name="openDocument">Opens a document through the shell's open-document path (never a URL).</param>
    /// <param name="store">Where read state is kept; a file under the local data folder when absent.</param>
    /// <param name="clock">The clock for toasts and relative times.</param>
    /// <param name="appIsFocused">Whether the app has focus (a toast is raised only then).</param>
    /// <param name="post">Runs an action on the UI thread.</param>
    public NotificationHost(
        ShellViewModel shell,
        Func<IRoomSession> sessionFactory,
        Func<string> serverId,
        Action<string, string> openDocument,
        INoticeStateStore? store = null,
        TimeProvider? clock = null,
        Func<bool>? appIsFocused = null,
        Action<Action>? post = null)
    {
        _shell = shell ?? throw new ArgumentNullException(nameof(shell));
        _sessionFactory = sessionFactory ?? throw new ArgumentNullException(nameof(sessionFactory));
        _serverId = serverId ?? throw new ArgumentNullException(nameof(serverId));
        _openDocument = openDocument ?? throw new ArgumentNullException(nameof(openDocument));
        _store = store ?? new FileNoticeStateStore(FileNoticeStateStore.DefaultRoot());
        _clock = clock ?? TimeProvider.System;
        _appIsFocused = appIsFocused ?? (() => true);
        _post = post ?? (action => Dispatcher.UIThread.Post(action));
        _shell.PropertyChanged += OnShellChanged;
        _shell.SignedOut += OnSignedOut;
        Start();
    }

    /// <summary>The attached notification centre, or null when no one is signed in.</summary>
    public NotificationSurface? Surface => _surface;

    /// <summary>How many forms are being watched now (equals the open form tabs with an id).</summary>
    public int ActiveWatches => _watcher?.ActiveWatches ?? 0;

    /// <summary>
    /// Gives an open form its staleness watch and bar; dispose the result when its tab closes. Null when
    /// no session is open (nothing to join).
    /// </summary>
    public IDisposable? WatchForm(DocumentFormViewModel form)
    {
        ArgumentNullException.ThrowIfNull(form);
        return _watcher is { } watcher ? new FormStaleness(form, watcher) : null;
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _shell.PropertyChanged -= OnShellChanged;
        _shell.SignedOut -= OnSignedOut;
        Stop();
    }

    private void OnShellChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName == nameof(ShellViewModel.UserId))
        {
            Start();
        }
    }

    private void OnSignedOut(object? sender, EventArgs e) => Stop();

    private void Start()
    {
        var user = _shell.UserId;
        if (user.Length == 0)
        {
            return;
        }

        var server = _serverId();
        if (string.IsNullOrWhiteSpace(server) || (_surface is not null && user == _user))
        {
            return;
        }

        Stop();
        var session = _sessionFactory();
        var feed = new NotificationFeed(session.Rooms, _store, server, user, _appIsFocused);
        var surface = new NotificationSurface(feed, _clock, _openDocument, _post);
        _session = session;
        _surface = surface;
        _watcher = new DocumentStalenessWatcher(session.Rooms, () => feed.AutoReloadClean);
        _user = user;
        surface.Start();
        _shell.AttachNotifications(surface);
    }

    private void Stop()
    {
        if (_surface is null)
        {
            return;
        }

        _shell.DetachNotifications();
        _surface.Dispose();
        _surface = null;
        _watcher = null;
        _user = string.Empty;
        var session = _session;
        _session = null;
        if (session is not null)
        {
            _ = session.DisposeAsync().AsTask();
        }
    }
}
