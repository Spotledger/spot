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
/// what happened and offers "Reload". For a dirty form the reload discards edits and asks first; when
/// the host gave a merge, a dirty form is also offered "Merge my changes", which keeps the person's
/// edits and lets them choose for each field that changed on both sides.
/// </summary>
public sealed class StalenessBar : UserControl, IDisposable
{
    private readonly DocumentWatch _watch;
    private readonly Func<Task<bool>> _confirmDiscard;
    private readonly TextBlock _message = NoticeLook.Text(string.Empty, 3);
    private readonly Func<Task>? _merge;
    private readonly Button _reload = new() { Content = "Reload" };
    private readonly Button _mergeButton = new() { Content = "Merge my changes" };
    private readonly List<IDisposable> _subscriptions = [];

    /// <summary>Creates the bar over one form's watch.</summary>
    /// <param name="watch">The form's watch.</param>
    /// <param name="confirmDiscard">Asks whether unsaved changes may be discarded by a reload.</param>
    /// <param name="merge">Starts the merge of the person's edits with the server's version; null offers none.</param>
    public StalenessBar(DocumentWatch watch, Func<Task<bool>> confirmDiscard, Func<Task>? merge = null)
    {
        _merge = merge;
        _watch = watch ?? throw new ArgumentNullException(nameof(watch));
        _confirmDiscard = confirmDiscard ?? throw new ArgumentNullException(nameof(confirmDiscard));
        IsVisible = false;
        AutomationProperties.SetLiveSetting(this, AutomationLiveSetting.Polite);
        _reload.Click += async (_, _) => await _watch.ReloadAsync(_confirmDiscard);
        _mergeButton.Click += async (_, _) =>
        {
            if (_merge is not null)
            {
                await _merge();
            }
        };
        DockPanel.SetDock(_reload, Dock.Right);
        DockPanel.SetDock(_mergeButton, Dock.Right);
        Content = new Border
        {
            Padding = new Thickness(12, 8),
            Child = new DockPanel { Children = { _reload, _mergeButton, _message } },
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
        StalenessKind.ChangedDirty => "Changed by someone else since you opened it. Reloading discards your changes; merging keeps them.",
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
        _mergeButton.IsVisible = kind == StalenessKind.ChangedDirty && _merge is not null;
        AutomationProperties.SetName(this, text);
    }
}
