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

    /// <summary>
    /// Test only. Drives the real picker and login view models, so the sign-in is the login screen's own
    /// <c>ApiConnection.LoginAsync</c> call. "Remember me" stays off, so only the in-memory session exists; the
    /// password lives in the login view model only while the command runs (it clears it in every path).
    /// </summary>
    public Task<bool> SignInAsync(string url, string user, string password) =>
        Dispatcher.UIThread.InvokeAsync(() => SignInOnUiAsync(url, user, password));

    private async Task<bool> SignInOnUiAsync(string url, string user, string password)
    {
        if (_stage.CurrentStage == AppStage.Shell)
        {
            return true;
        }

        if (_stage.CurrentStage == AppStage.Login)
        {
            _stage.Login.BackCommand.Execute(null);
        }

        var picker = _stage.ServerPicker;
        picker.ServerUrl = url;
        picker.CheckCommand.Execute(null);
        await picker.CheckCommand.ExecutionTask.ConfigureAwait(true);
        if (!picker.IsReady)
        {
            return false;
        }

        var leftPicker = WaitForStageAsync(s => s != AppStage.ServerPicker);
        picker.NextCommand.Execute(null);
        if (await leftPicker.ConfigureAwait(true) != AppStage.Login)
        {
            // The shell (a resumed session) or a timeout: only the shell counts as signed in.
            return _stage.CurrentStage == AppStage.Shell;
        }

        var login = _stage.Login;
        login.RememberMe = false;
        login.User = user;
        login.Password = password;
        login.SignInCommand.Execute(null);
        await login.SignInCommand.ExecutionTask.ConfigureAwait(true);
        login.Password = string.Empty;
        if (_stage.CurrentStage != AppStage.Shell)
        {
            return false;
        }

        await _stage.SessionInitialization.ConfigureAwait(true);
        return _stage.CurrentStage == AppStage.Shell;
    }

    /// <summary>Waits (at most ten seconds) for the stage to satisfy <paramref name="done"/>; returns the stage then.</summary>
    private async Task<AppStage> WaitForStageAsync(Func<AppStage, bool> done)
    {
        var tcs = new TaskCompletionSource<AppStage>(TaskCreationOptions.RunContinuationsAsynchronously);
        void Handler(object? sender, System.ComponentModel.PropertyChangedEventArgs e)
        {
            if (e.PropertyName == nameof(AppViewModel.CurrentStage) && done(_stage.CurrentStage))
            {
                tcs.TrySetResult(_stage.CurrentStage);
            }
        }

        _stage.PropertyChanged += Handler;
        try
        {
            if (done(_stage.CurrentStage))
            {
                return _stage.CurrentStage;
            }

            var finished = await Task.WhenAny(tcs.Task, Task.Delay(TimeSpan.FromSeconds(10))).ConfigureAwait(true);
            return finished == tcs.Task ? tcs.Task.Result : _stage.CurrentStage;
        }
        finally
        {
            _stage.PropertyChanged -= Handler;
        }
    }

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
