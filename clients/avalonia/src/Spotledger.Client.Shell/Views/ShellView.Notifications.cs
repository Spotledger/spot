using System.Diagnostics;
using Avalonia.Automation;
using Avalonia.Controls;

namespace Spotledger.Client.Shell.Views;

/// <summary>
/// The notification half of the shell view (WP-UI-02.T6): the bell opens a flyout whose content the
/// attached <see cref="INotificationSurface"/> built, the toast sits in the host in the bottom corner,
/// and the bell's accessible name follows the feed. The view owns no notification logic: the bell press
/// is the <c>view.notifications</c> command like its menu item and chord.
/// </summary>
public sealed partial class ShellView
{
    private Button? _bell;
    private ContentControl? _toastHost;
    private Flyout? _notificationsFlyout;
    private long _flyoutClosedAt;

    private void ConnectNotifications(INameScope scope)
    {
        _bell = scope.Get<Button>("BellButton");
        _toastHost = scope.Get<ContentControl>("ToastHost");
        if (_bell is null)
        {
            return;
        }

        _bell.Click += (_, _) =>
        {
            // A press on the bell while the flyout is open first light-dismisses it, and the click then
            // arrives here: that click closed it, so it must not open it again.
            if (Stopwatch.GetElapsedTime(_flyoutClosedAt) < TimeSpan.FromMilliseconds(250))
            {
                return;
            }

            Fire(ShellViewModel.NotificationsCommandId, CommandRoute.ContextMenu);
        };
    }

    /// <summary>The flyout under the bell; null until it has been opened once.</summary>
    internal Flyout? NotificationsFlyoutControl => _notificationsFlyout;

    /// <summary>The control that hosts the toast.</summary>
    internal ContentControl ToastHostControl => _toastHost!;

    private void ApplyNotificationSurface()
    {
        if (_shell is not { } shell)
        {
            return;
        }

        if (_bell is not null)
        {
            AutomationProperties.SetName(_bell, shell.NotificationBellName);
        }

        if (_toastHost is not null)
        {
            _toastHost.Content = shell.Notifications?.Toast;
        }

        SyncNotificationsFlyout(shell);
    }

    private void SyncNotificationsFlyout(ShellViewModel shell)
    {
        if (_bell is null)
        {
            return;
        }

        if (shell.IsNotificationsOpen && shell.Notifications?.Flyout is Control content)
        {
            if (_notificationsFlyout is { IsOpen: true })
            {
                return;
            }

            _notificationsFlyout ??= CreateNotificationsFlyout();
            _notificationsFlyout.Content = content;
            _notificationsFlyout.ShowAt(_bell);
        }
        else if (_notificationsFlyout is { IsOpen: true } open)
        {
            open.Hide();
        }
    }

    private Flyout CreateNotificationsFlyout()
    {
        var flyout = new Flyout { Placement = PlacementMode.BottomEdgeAlignedRight };
        flyout.Closed += (_, _) =>
        {
            _flyoutClosedAt = Stopwatch.GetTimestamp();
            // Let go of the content so the next opening can host the same control again.
            flyout.Content = null;
            if (_shell is { IsNotificationsOpen: true } shell)
            {
                shell.IsNotificationsOpen = false;
            }
        };
        return flyout;
    }
}
