namespace Spotledger.Client.Shell;

/// <summary>
/// What the shell needs from the notification centre (WP-UI-02.T6). The centre itself lives in
/// <c>Spotledger.Client.Notifications</c>, which depends on this project, so the shell sees it only
/// through this seam: the unread count and the bell's accessible name, the two surfaces it hosts (the
/// flyout under the bell and the toast in the bottom corner) and the two settings the Tools menu toggles.
/// </summary>
public interface INotificationSurface
{
    /// <summary>How many notices are unread (the bell badge).</summary>
    int UnreadCount { get; }

    /// <summary>The bell's accessible name, such as "Notifications, 3 unread".</summary>
    string BellName { get; }

    /// <summary>The control shown in the flyout under the bell (an Avalonia control), or null.</summary>
    object? Flyout { get; }

    /// <summary>The control that shows the toast in the bottom corner (an Avalonia control), or null.</summary>
    object? Toast { get; }

    /// <summary>The "do not disturb" setting: toasts off, the bell still counts.</summary>
    bool DoNotDisturb { get; set; }

    /// <summary>The "reload unedited forms by themselves" setting.</summary>
    bool AutoReloadClean { get; set; }

    /// <summary>Raised on the UI thread after the count, the name or a setting changed.</summary>
    event EventHandler? Changed;

    /// <summary>Raised on the UI thread when the flyout should close by itself (a notice's link was opened).</summary>
    event EventHandler? CloseRequested;
}

/// <summary>
/// The notification half of <see cref="ShellViewModel"/>: the bell follows the attached
/// <see cref="INotificationSurface"/>, <c>view.notifications</c> opens and closes the flyout, and the
/// two settings are commands in the Tools menu (the shell has no settings surface yet).
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The id of the command that opens and closes the notifications flyout.</summary>
    public const string NotificationsCommandId = "view.notifications";

    /// <summary>The id of the command that toggles do-not-disturb.</summary>
    public const string DoNotDisturbCommandId = "notifications.doNotDisturb";

    /// <summary>The id of the command that toggles reloading unedited forms.</summary>
    public const string AutoReloadCommandId = "notifications.autoReload";

    private INotificationSurface? _notifications;
    private string _notificationBellName = "Notifications";
    private bool _isNotificationsOpen;

    /// <summary>The attached notification centre, or null before sign-in and after sign-out.</summary>
    public INotificationSurface? Notifications => _notifications;

    /// <summary>The bell's accessible name: the feed's ("Notifications, 3 unread") while one is attached.</summary>
    public string NotificationBellName
    {
        get => _notificationBellName;
        private set => SetField(ref _notificationBellName, value);
    }

    /// <summary>True while the notifications flyout is open. The view opens and closes the popup to match
    /// and writes false when the person dismisses it.</summary>
    public bool IsNotificationsOpen
    {
        get => _isNotificationsOpen;
        set
        {
            if (value && _notifications is null)
            {
                return;
            }

            SetField(ref _isNotificationsOpen, value);
        }
    }

    /// <summary>
    /// Attaches the notification centre of the signed-in user: the bell shows its unread count and name,
    /// the view hosts its flyout and toast. Replaces an earlier one. Call on the UI thread.
    /// </summary>
    public void AttachNotifications(INotificationSurface surface)
    {
        ArgumentNullException.ThrowIfNull(surface);
        DetachNotifications();
        _notifications = surface;
        surface.Changed += OnNotificationsChanged;
        surface.CloseRequested += OnNotificationsCloseRequested;
        OnPropertyChanged(nameof(Notifications));
        ApplyNotifications();
    }

    /// <summary>Forgets the notification centre (sign-out): the bell goes quiet and the flyout closes. The host disposes it.</summary>
    public void DetachNotifications()
    {
        if (_notifications is { } surface)
        {
            surface.Changed -= OnNotificationsChanged;
            surface.CloseRequested -= OnNotificationsCloseRequested;
        }

        _notifications = null;
        IsNotificationsOpen = false;
        NotificationCount = 0;
        NotificationBellName = "Notifications";
        OnPropertyChanged(nameof(Notifications));
    }

    /// <summary>Opens the flyout when it is closed and closes it when it is open.</summary>
    public void ToggleNotifications()
    {
        if (_notifications is null)
        {
            StatusMessage = "Notifications are not connected.";
            return;
        }

        IsNotificationsOpen = !IsNotificationsOpen;
    }

    private void OnNotificationsChanged(object? sender, EventArgs e) => ApplyNotifications();

    private void OnNotificationsCloseRequested(object? sender, EventArgs e) => IsNotificationsOpen = false;

    private void ApplyNotifications()
    {
        if (_notifications is not { } surface)
        {
            return;
        }

        NotificationCount = surface.UnreadCount;
        NotificationBellName = surface.BellName;
    }

    private void RegisterNotificationHandlers()
    {
        Dispatcher.Register(NotificationsCommandId, _ =>
        {
            ToggleNotifications();
            return Task.CompletedTask;
        });
        Dispatcher.Register(DoNotDisturbCommandId, _ =>
        {
            if (_notifications is { } surface)
            {
                surface.DoNotDisturb = !surface.DoNotDisturb;
                StatusMessage = surface.DoNotDisturb ? "Do not disturb is on. The bell still counts." : "Do not disturb is off.";
            }
            else
            {
                StatusMessage = "Notifications are not connected.";
            }

            return Task.CompletedTask;
        });
        Dispatcher.Register(AutoReloadCommandId, _ =>
        {
            if (_notifications is { } surface)
            {
                surface.AutoReloadClean = !surface.AutoReloadClean;
                StatusMessage = surface.AutoReloadClean
                    ? "Open forms you have not edited reload by themselves when someone else changes them."
                    : "Open forms ask before reloading.";
            }
            else
            {
                StatusMessage = "Notifications are not connected.";
            }

            return Task.CompletedTask;
        });
    }
}
