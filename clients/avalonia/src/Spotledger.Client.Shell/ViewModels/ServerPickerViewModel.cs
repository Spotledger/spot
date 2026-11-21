using System.Collections.ObjectModel;
using Spotledger.Client.Api;

namespace Spotledger.Client.Shell;

/// <summary>
/// Step one of the app: enter or select a server URL and check it with
/// <c>GET /api/health/ready</c>. Recent servers come from <see cref="RecentServersStore"/>.
/// A ready server enables <see cref="NextCommand"/>; the app then moves to login.
/// </summary>
public sealed class ServerPickerViewModel : ObservableObject
{
    private string _serverUrl = string.Empty;
    private string? _statusMessage;
    private bool _isReady;
    private readonly ApiConnection _connection;
    private readonly RecentServersStore _recentServers;

    public ServerPickerViewModel(ApiConnection connection, RecentServersStore recentServers)
    {
        _connection = connection ?? throw new ArgumentNullException(nameof(connection));
        _recentServers = recentServers ?? throw new ArgumentNullException(nameof(recentServers));
        RecentServers = new ObservableCollection<string>(_recentServers.Load());
        CheckCommand = new AsyncRelayCommand(CheckAsync, _ => CanCheck);
        NextCommand = new RelayCommand(_ => GoToLogin(), _ => IsReady);
    }

    /// <summary>The URL under consideration.</summary>
    public string ServerUrl
    {
        get => _serverUrl;
        set
        {
            if (SetField(ref _serverUrl, value))
            {
                // Editing the URL invalidates any previous ready verdict.
                IsReady = false;
                StatusMessage = null;
                CheckCommand.RaiseCanExecuteChanged();
                NextCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>Remembered server URLs, most recent first.</summary>
    public ObservableCollection<string> RecentServers { get; }

    /// <summary>Ready/not-ready/unreachable feedback for the form.</summary>
    public string? StatusMessage
    {
        get => _statusMessage;
        private set
        {
            SetField(ref _statusMessage, value);
            OnPropertyChanged(nameof(HasStatusMessage));
        }
    }

    public bool HasStatusMessage => !string.IsNullOrEmpty(StatusMessage);

    /// <summary>True when the last check against the current URL came back ready.</summary>
    public bool IsReady
    {
        get => _isReady;
        private set
        {
            SetField(ref _isReady, value);
            NextCommand.RaiseCanExecuteChanged();
        }
    }

    /// <summary>The non-blocking version notice when the server is newer than this app.</summary>
    public string? VersionNotice
    {
        get => _versionNotice;
        private set
        {
            SetField(ref _versionNotice, value);
            OnPropertyChanged(nameof(HasVersionNotice));
        }
    }

    public bool HasVersionNotice => !string.IsNullOrEmpty(VersionNotice);

    private string? _versionNotice;

    public AsyncRelayCommand CheckCommand { get; }

    public RelayCommand NextCommand { get; }

    /// <summary>Raised when a ready server is accepted; the app switches to login.</summary>
    public event EventHandler? ReadyToLogin;

    /// <summary>A user picking one of the recent URLs from the dropdown.</summary>
    public void SelectRecentServer(string url)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(url);
        ServerUrl = url;
    }

    private bool CanCheck => Uri.TryCreate(ServerUrl?.Trim() ?? string.Empty, UriKind.Absolute, out var uri)
        && (uri.Scheme == Uri.UriSchemeHttp || uri.Scheme == Uri.UriSchemeHttps);

    private async Task CheckAsync(object? parameter)
    {
        if (!Uri.TryCreate(ServerUrl.Trim(), UriKind.Absolute, out var uri)
            || (uri.Scheme != Uri.UriSchemeHttp && uri.Scheme != Uri.UriSchemeHttps))
        {
            IsReady = false;
            StatusMessage = "Enter a server URL starting with http:// or https://.";
            return;
        }

        _connection.Connect(uri);
        var result = await _connection.CheckReadyAsync();
        if (!result.IsReady)
        {
            IsReady = false;
            StatusMessage = result.Detail ?? "The server is not ready.";
            return;
        }

        // The version negotiation rides on the same check (no second round of waiting for
        // the user): a major mismatch refuses the server, a newer minor leaves a
        // non-blocking notice, an older minor resolves what the server cannot do.
        ServerCapabilities capabilities;
        try
        {
            capabilities = await _connection.NegotiateVersionAsync();
        }
        catch (ApiVersionMismatchException mismatch)
        {
            IsReady = false;
            StatusMessage = mismatch.Message;
            return;
        }

        IsReady = true;
        VersionNotice = capabilities.Notice;
        StatusMessage = "The server is ready.";
        _recentServers.Remember(uri.GetLeftPart(UriPartial.Authority));
        RecentServers.Clear();
        foreach (var recent in _recentServers.Load())
        {
            RecentServers.Add(recent);
        }
    }

    private void GoToLogin()
    {
        if (IsReady)
        {
            ReadyToLogin?.Invoke(this, EventArgs.Empty);
        }
    }
}
