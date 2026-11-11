using System.Collections.ObjectModel;
using Spotledger.Client.Api;

namespace Spotledger.Client.Shell;

/// <summary>
/// The "My sessions" dialog (WP-UI-02.T3): the signed-in user's open sessions from
/// <c>GET /api/v1/auth/sessions</c>, each with an "End session" action
/// (<c>DELETE /api/v1/auth/sessions/{id}</c>; contract v0.3.0 — the connection's
/// hand-written plumbing). The list is loaded when the dialog opens; the current session
/// is marked instead of endable — the shell's Sign out covers that one.
/// </summary>
public sealed class MySessionsViewModel : ObservableObject
{
    private string? _loadError;
    private bool _isLoaded;
    private readonly ApiConnection _connection;

    public MySessionsViewModel(ApiConnection connection)
    {
        _connection = connection ?? throw new ArgumentNullException(nameof(connection));
        EndSessionCommand = new AsyncRelayCommand(EndSessionAsync, parameter => parameter is ClientSessionSummary summary && !summary.Current);
        CloseCommand = new RelayCommand(_ => Close());
    }

    /// <summary>Raised after the dialog was dismissed; the view closes.</summary>
    public event EventHandler? CloseRequested;

    public ObservableCollection<ClientSessionSummary> Sessions { get; } = new();

    public string? LoadError
    {
        get => _loadError;
        private set
        {
            SetField(ref _loadError, value);
            OnPropertyChanged(nameof(HasLoadError));
        }
    }

    public bool HasLoadError => !string.IsNullOrEmpty(LoadError);

    public bool IsLoaded
    {
        get => _isLoaded;
        private set => SetField(ref _isLoaded, value);
    }

    public AsyncRelayCommand EndSessionCommand { get; }

    public RelayCommand CloseCommand { get; }

    /// <summary>Loads the user's sessions (when the dialog opens, never polled).</summary>
    public async Task LoadAsync(CancellationToken cancellationToken = default)
    {
        LoadError = null;
        try
        {
            var sessions = await _connection.ListMySessionsAsync(cancellationToken).ConfigureAwait(true);
            Sessions.Clear();
            foreach (var session in sessions)
            {
                Sessions.Add(session);
            }

            IsLoaded = true;
        }
        catch (ApiRequestException failure)
        {
            HasSessions = false;
            LoadError = failure.Error.Message;
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            HasSessions = false;
            LoadError = "The sessions could not be loaded. Try again.";
        }
    }

    private bool _hasSessions;

    public bool HasSessions
    {
        get => _hasSessions;
        private set => SetField(ref _hasSessions, value);
    }

    private async Task EndSessionAsync(object? parameter)
    {
        if (parameter is not ClientSessionSummary summary || summary.Current)
        {
            return;
        }

        try
        {
            await _connection.EndMySessionAsync(summary.Id).ConfigureAwait(true);
            Sessions.Remove(summary);
        }
        catch (ApiRequestException failure)
        {
            LoadError = failure.Error.Message;
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            LoadError = "The session could not be ended. Try again.";
        }
    }

    private void Close() => CloseRequested?.Invoke(this, EventArgs.Empty);
}