using System.ComponentModel;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Spotledger.Client.Forms;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.Workspace;

namespace Spotledger.Client.Desktop.Hosting;

/// <summary>
/// Connects a built <see cref="ShellView"/> to the content tabs the host opened, using only the view's
/// named parts: the tab content area shows the view of a tab's content (a document form, the recover
/// drafts list) instead of the placeholder text, and the shell learns whether a text box has the focus
/// (so Ctrl+Z stays the text box's own while it does). Nothing in the view itself is edited.
/// </summary>
internal sealed class ShellViewHooks : IDisposable
{
    private readonly ShellView _view;
    private readonly ShellViewModel _shell;
    private readonly ContentControl? _content;
    private readonly Dictionary<WorkspaceTabViewModel, Control> _views = [];
    private readonly IDisposable? _contentWatch;
    private bool _applying;

    public ShellViewHooks(ShellView view, ShellViewModel shell)
    {
        _view = view ?? throw new ArgumentNullException(nameof(view));
        _shell = shell ?? throw new ArgumentNullException(nameof(shell));
        _content = view.FindControl<ContentControl>("TabContent");
        _shell.PropertyChanged += OnShellChanged;
        _shell.TabClosed += OnTabClosed;
        _contentWatch = _content?.GetObservable(ContentControl.ContentProperty)
            .Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(_ => Apply()));
        _view.AddHandler(InputElement.GotFocusEvent, OnFocusChanged, Avalonia.Interactivity.RoutingStrategies.Bubble | Avalonia.Interactivity.RoutingStrategies.Tunnel);
        _view.AddHandler(InputElement.LostFocusEvent, OnFocusChanged, Avalonia.Interactivity.RoutingStrategies.Bubble | Avalonia.Interactivity.RoutingStrategies.Tunnel);
        Apply();
    }

    /// <summary>The view shown for a tab's content, or null when the tab has none.</summary>
    public Control? ViewOf(WorkspaceTabViewModel tab) => _views.GetValueOrDefault(tab);

    public void Dispose()
    {
        _shell.PropertyChanged -= OnShellChanged;
        _shell.TabClosed -= OnTabClosed;
        _contentWatch?.Dispose();
        _view.RemoveHandler(InputElement.GotFocusEvent, OnFocusChanged);
        _view.RemoveHandler(InputElement.LostFocusEvent, OnFocusChanged);
        foreach (var control in _views.Values)
        {
            (control as IDisposable)?.Dispose();
        }

        _views.Clear();
    }

    private void OnShellChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName == nameof(ShellViewModel.SelectedTab))
        {
            Apply();
        }
    }

    private void OnTabClosed(object? sender, TabClosedEventArgs e)
    {
        if (_views.Remove(e.Tab, out var control))
        {
            (control as IDisposable)?.Dispose();
        }
    }

    /// <summary>Puts the selected content tab's view into the content area. The shell view writes its
    /// placeholder on every tab change; this runs after it and puts the content back.</summary>
    private void Apply()
    {
        if (_applying || _content is null || _shell.SelectedTab is not { Content: { } content } tab)
        {
            return;
        }

        if (!_views.TryGetValue(tab, out var control))
        {
            control = content switch
            {
                DocumentFormViewModel form => new DocumentFormView(form),
                RecoverDraftsViewModel list => new RecoverDraftsView { DataContext = list },
                _ => null!,
            };
            if (control is null)
            {
                return;
            }

            _views[tab] = control;
        }

        if (ReferenceEquals(_content.Content, control))
        {
            return;
        }

        _applying = true;
        try
        {
            _content.Content = control;
        }
        finally
        {
            _applying = false;
        }
    }

    private void OnFocusChanged(object? sender, Avalonia.Interactivity.RoutedEventArgs e)
    {
        var focused = TopLevel.GetTopLevel(_view)?.FocusManager?.GetFocusedElement();
        _shell.IsFocusedText = UndoShortcuts.IsTextInput(focused);
    }
}
