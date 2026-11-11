using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Spotledger.Client.Api;
using Spotledger.Client.Desktop.Hosting;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.Theme;
using Spotledger.Client.Workspace;

namespace Spotledger.Client.Desktop;

/// <summary>
/// The desktop window: hosts the AppViewModel stage machine (server picker → login →
/// shell). The token gallery stays reachable with <c>--gallery</c> (WP-UI-15.T1).
/// </summary>
public sealed class MainWindow : Window, IDisposable
{
    private readonly ContentControl _host = new();
    private readonly DraftSessionHost _drafts;
    private readonly WorkspaceHost _workspace;
    private ShellViewHooks? _hooks;

    public MainWindow()
    {
        Title = "Spotledger";
        Width = 1280;
        Height = 800;
        Connection = new ApiConnection();
        Stage = new AppViewModel(Connection, new RecentServersStore());
        var draftRoot = FileDraftStoreOptions.DefaultRoot();
        _drafts = new DraftSessionHost(
            _ => new FileDraftStore(new FileDraftStoreOptions { Root = draftRoot }, Connection.Secrets),
            Path.Combine(draftRoot, "pending-wipe"));
        _workspace = new WorkspaceHost(
            Stage.Shell,
            _drafts,
            new NativeDocumentGateway(Connection, new MetaCache(Connection)),
            () => Connection.BaseUri?.GetLeftPart(UriPartial.Authority) ?? string.Empty);
        Deactivated += (_, _) => _ = _workspace.FlushAllAsync();
        Closing += (_, _) => FlushOnClose();
        Closed += (_, _) => Dispose();
        Background = new SolidColorBrush(GetColor(Tokens.TokenColorSurfaceCanvas));
        Content = _host;
        ShowStage(Stage.CurrentStage);
        Stage.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName == nameof(AppViewModel.CurrentStage))
            {
                ShowStage(Stage.CurrentStage);
            }
        };
    }

    /// <summary>Releases the workspace wiring and the draft session (the drafts stay on disk).</summary>
    public void Dispose()
    {
        _hooks?.Dispose();
        _hooks = null;
        _workspace.Dispose();
        _drafts.Dispose();
    }

    /// <summary>The stage machine driving this window.</summary>
    public AppViewModel Stage { get; }

    /// <summary>The API connection shared by the stage machine.</summary>
    public ApiConnection Connection { get; }

    private void ShowStage(AppStage stage)
    {
        _hooks?.Dispose();
        _hooks = null;
        switch (stage)
        {
            case AppStage.Shell:
                var shell = new ShellView { DataContext = Stage.Shell };
                _hooks = new ShellViewHooks(shell, Stage.Shell);
                _host.Content = shell;
                break;
            case AppStage.ServerPicker:
                _host.Content = new ServerPickerView { DataContext = Stage.ServerPicker };
                break;
            case AppStage.Login:
                _host.Content = new LoginView { DataContext = Stage.Login };
                break;
            default:
                _host.Content = null;
                break;
        }
    }

    /// <summary>The window is closing: unwritten drafts go to disk (closing without signing out keeps them).</summary>
    private void FlushOnClose()
    {
        try
        {
            _ = Task.Run(_workspace.FlushAllAsync).Wait(TimeSpan.FromSeconds(3));
        }
        catch (AggregateException)
        {
            // A failed last write leaves the previous draft file; the next start offers it.
        }
    }

    private static Color GetColor(string tokenKey) =>
        Application.Current?.TryFindResource(tokenKey, out var value) == true && value is Color color
            ? color
            : Colors.Transparent;
}
