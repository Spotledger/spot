using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Browser;

/// <summary>
/// Hosts the shell stage machine in the browser: server picker, then login, then the shell home. It is
/// the browser twin of the desktop <c>MainWindow</c> reduced to what the first paint needs. The session
/// token is kept in memory only (a browser has no OS secret store; <c>InMemorySecretStore</c>), so a
/// reload signs the user out; the recent-server list survives through <see cref="RecentServersBridge"/>.
/// </summary>
internal sealed class BrowserShellHost : ContentControl, IWorkspaceOpener, IDisposable
{
    private readonly BrowserHostState _state;
    private readonly ApiConnection _connection;
    private readonly AppViewModel _stage;
    private bool _firstPaintMarked;

    public BrowserShellHost(BrowserHostState state)
    {
        _state = state;
        _connection = new ApiConnection(secretStore: new InMemorySecretStore());
        _stage = new AppViewModel(_connection, new RecentServersStore(state.RecentServers.FilePath));
        Background = new SolidColorBrush(Application.Current?.TryFindResource(Tokens.TokenColorSurfaceCanvas, out var value) == true && value is Color color
            ? color
            : Colors.Transparent);
        state.AttachOpener(this);
        ShowStage(_stage.CurrentStage);
        _stage.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName == nameof(AppViewModel.CurrentStage))
            {
                _state.RecentServers.Persist();
                ShowStage(_stage.CurrentStage);
            }
        };
    }

    public void Dispose() => _connection.Dispose();

    public Task<bool> OpenFormAsync(string doctype, string id) =>
        OpenTabAsync($"{doctype} {id}".TrimEnd(), $"form:{doctype}:{id}");

    public Task<bool> OpenListAsync(string doctype) =>
        OpenTabAsync(doctype, $"list:{doctype}");

    protected override void OnAttachedToVisualTree(VisualTreeAttachmentEventArgs e)
    {
        base.OnAttachedToVisualTree(e);
        // The first frame is rendered after the next layout pass: mark it, then mark the shell ready.
        Dispatcher.UIThread.Post(MarkFirstFrame, DispatcherPriority.Background);
    }

    private void MarkFirstFrame()
    {
        if (_firstPaintMarked)
        {
            return;
        }

        _firstPaintMarked = true;
        _state.MarkFirstPaint();
        _state.MarkReady();
        SpotBrowserJs.SignalReady();
        _ = LoadCalculationModuleAsync();
    }

    /// <summary>After the shell is interactive, looks for the optional calculation module and switches to it.</summary>
    private async Task LoadCalculationModuleAsync()
    {
        var module = new JsWasmCalcModule();
        if (await JsWasmCalcModule.LoadAsync("./calc/spot_calc.wasm"))
        {
            _state.Calculation.UseModule(module);
        }
    }

    /// <summary>Opens a tab on the shell. Until the lazy module registry (WP-UI-09.T2) provides the real
    /// form and list views, the tab holds a short description; the tab, its key and the metrics are real.</summary>
    private Task<bool> OpenTabAsync(string title, string key) =>
        Dispatcher.UIThread.InvokeAsync(() =>
        {
            if (_stage.CurrentStage != AppStage.Shell)
            {
                return false;
            }

            _stage.Shell.OpenContentTab(title, $"{title} (opens when its module has loaded)", key);
            return true;
        }).GetTask();

    private void ShowStage(AppStage stage)
    {
        Content = stage switch
        {
            AppStage.Shell => new ShellView { DataContext = _stage.Shell },
            AppStage.ServerPicker => new ServerPickerView { DataContext = _stage.ServerPicker },
            AppStage.Login => new LoginView { DataContext = _stage.Login },
            _ => null,
        };
    }
}
