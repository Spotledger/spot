using Spotledger.Client.Api;
using Spotledger.Client.Notifications;
using Spotledger.Client.Realtime;

namespace Spotledger.Client.Desktop.Hosting;

/// <summary>The rooms of one signed-in session and what ends them.</summary>
internal interface IRoomSession : IAsyncDisposable
{
    /// <summary>Where the notification feed and the open forms join rooms.</summary>
    IRoomSource Rooms { get; }
}

/// <summary>
/// The realtime connection of one session (WP-UI-02.T6): one <see cref="RealtimeClient"/> per sign-in,
/// connected with the session token read from the connection at each attempt (the client keeps no
/// credential). Realtime is a hint channel, so a failed connection only means no live updates: the
/// shell says so on the status bar and everything else works.
/// </summary>
internal sealed class RealtimeRoomSession : IRoomSession
{
    private readonly RealtimeClient _client;
    private readonly CancellationTokenSource _lifetime = new();

    public RealtimeRoomSession(ApiConnection connection, Action<string>? status = null)
    {
        ArgumentNullException.ThrowIfNull(connection);
        var site = connection.BaseUri ?? throw new InvalidOperationException("No server is selected.");
        _client = new RealtimeClient(site, new RealtimeClientOptions { HandlerContext = SynchronizationContext.Current });
        Rooms = new RealtimeRoomSource(_client);
        _ = ConnectAsync(connection, status);
    }

    public IRoomSource Rooms { get; }

    public async ValueTask DisposeAsync()
    {
        await _lifetime.CancelAsync();
        try
        {
            await _client.DisposeAsync();
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            // Leaving is best effort: the server drops the connection with the session anyway.
        }

        _lifetime.Dispose();
    }

    private async Task ConnectAsync(ApiConnection connection, Action<string>? status)
    {
        try
        {
            await _client.ConnectAsync(_ => ValueTask.FromResult(connection.Tokens.GetTokenForSecretStore()), _lifetime.Token);
        }
        catch (OperationCanceledException)
        {
            // The session ended before the connection was ready.
        }
        catch (Exception ex) when (ex is not OutOfMemoryException)
        {
            status?.Invoke("Live updates are off: the server could not be reached for notifications.");
        }
    }
}
