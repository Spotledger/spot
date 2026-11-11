using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Notifications;

/// <summary>
/// The inline bar above an open form. It never reloads by itself (unless the setting says so): it says
/// what happened and offers "Reload". For a dirty form the reload discards edits and asks first.
/// </summary>
public sealed class StalenessBar : UserControl, IDisposable
{
    private readonly DocumentWatch _watch;
    private readonly Func<Task<bool>> _confirmDiscard;
    private readonly TextBlock _message = NoticeLook.Text(string.Empty, 3);
    private readonly Button _reload = new() { Content = "Reload" };
    private readonly List<IDisposable> _subscriptions = [];

    /// <summary>Creates the bar over one form's watch.</summary>
    /// <param name="watch">The form's watch.</param>
    /// <param name="confirmDiscard">Asks whether unsaved changes may be discarded by a reload.</param>
    public StalenessBar(DocumentWatch watch, Func<Task<bool>> confirmDiscard)
    {
        _watch = watch ?? throw new ArgumentNullException(nameof(watch));
        _confirmDiscard = confirmDiscard ?? throw new ArgumentNullException(nameof(confirmDiscard));
        IsVisible = false;
        AutomationProperties.SetLiveSetting(this, AutomationLiveSetting.Polite);
        _reload.Click += async (_, _) => await _watch.ReloadAsync(_confirmDiscard);
        DockPanel.SetDock(_reload, Dock.Right);
        Content = new Border
        {
            Padding = new Thickness(12, 8),
            Child = new DockPanel { Children = { _reload, _message } },
        };
        _subscriptions.Add(NoticeLook.BindBrush(this, Tokens.TokenColorTextPrimary, brush => Foreground = brush));
        _subscriptions.Add(NoticeLook.BindBrush(this, Tokens.TokenColorSurfaceSunken, brush => Background = brush));
        _watch.Changed += OnChanged;
        Render();
    }

    /// <summary>The words for a state; empty for none.</summary>
    public static string MessageFor(StalenessKind kind) => kind switch
    {
        StalenessKind.ChangedClean => "This document was changed by someone else.",
        StalenessKind.ChangedDirty => "Changed by someone else since you opened it. Reloading discards your changes.",
        StalenessKind.Cancelled => "This document was cancelled by someone else.",
        StalenessKind.Deleted => "This document was deleted by someone else.",
        StalenessKind.MaybeStale => "Updates were missed while you were offline. Reload to be sure this is current.",
        _ => string.Empty,
    };

    /// <inheritdoc/>
    public void Dispose()
    {
        _watch.Changed -= OnChanged;
        foreach (var subscription in _subscriptions)
        {
            subscription.Dispose();
        }
    }

    private void OnChanged(object? sender, EventArgs e) => Dispatcher.UIThread.Post(Render);

    private void Render()
    {
        var kind = _watch.Kind;
        var text = MessageFor(kind);
        IsVisible = kind != StalenessKind.None;
        _message.Text = text;
        _reload.Content = kind == StalenessKind.ChangedDirty ? "Reload (discard my changes)" : "Reload";
        _reload.IsVisible = kind is not (StalenessKind.Deleted or StalenessKind.Cancelled);
        AutomationProperties.SetName(this, text);
    }
}
