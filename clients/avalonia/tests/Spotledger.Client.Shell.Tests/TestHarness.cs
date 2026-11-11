using System.Net;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// Shared plumbing for the fake-server tests: one <see cref="ApiConnection"/> wired to a
/// <see cref="FakeServerHandler"/>, a per-test recent-servers file under the temp directory,
/// and the <see cref="AppViewModel"/> stage machine on top. No network. The secret store is
/// the in-memory fake and the remembered-user hint file lives in the temp directory, so
/// tests can assert exactly what "Remember me" wrote (token only, never a password).
/// </summary>
internal sealed class TestHarness : IDisposable
{
    public const string ServerUrl = "http://127.0.0.55:8080";

    /// <summary>The contract's example session token from FakeServerHandler's login body.</summary>
    public const string SessionToken = "3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs";

    /// <summary>
    /// The token a test's re-sign-in provider installs. Deliberately NOT
    /// <see cref="SessionToken"/>: when the fresh session carries its own value, a recorded
    /// request's header says which session it was sent under, so a test can tell the
    /// expired attempts from the retries without relying on the order they arrived in.
    /// </summary>
    public const string RefreshedSessionToken = "r3-9FgBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs";

    /// <summary>The contract's <c>GET /api/v1/auth/session</c> body (one user, one site).</summary>
    public const string SessionBody = """
        {"data":{"expires_at":"2026-09-30T08:00:00Z","user":{"id":"0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f","email":"ayesha@acme.test","full_name":"Ayesha Khan"},"site":"acme.test"}}
        """;

    /// <summary>The bearer header value a request carries when the store holds <paramref name="token"/>.</summary>
    public static string BearerFor(string token) => "Bearer " + token;

    /// <summary>
    /// A session endpoint that answers 200 only to a request carrying <paramref name="token"/>
    /// and the session-expired 401 to every other request. Reading the token off the request
    /// makes "the new session is live" one atomic fact instead of two writes a test has to
    /// order: a request the token store stamped with the fresh token can never be answered
    /// with a 401, and a request stamped with the old token or none can never be answered
    /// with a 200 — whatever interleaving the two concurrent calls produce.
    /// </summary>
    public static Func<HttpRequestMessage, HttpResponseMessage> SessionAnsweringOnlyWith(string token)
    {
        var expected = BearerFor(token);
        return request => request.Headers.TryGetValues("Authorization", out var values)
            && values.Any(value => string.Equals(value, expected, StringComparison.Ordinal))
            ? FakeServerHandler.Json(HttpStatusCode.OK, SessionBody)
            : FakeServerHandler.Error(
                HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");
    }

    public const string DefaultUser = "ayesha@acme.test";

    /// <summary>The fake's sign-in password — never seen by the secret store.</summary>
    public const string DefaultPassword = "s3cret-passphrase";

    private readonly string _tempDirectory;

    public TestHarness(bool seedRecentFile = false)
    {
        _tempDirectory = Path.Combine(Path.GetTempPath(), "spotledger-shell-tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_tempDirectory);
        var recentPath = Path.Combine(_tempDirectory, "recent-servers.json");
        if (seedRecentFile)
        {
            File.WriteAllText(recentPath, $$"""["{{ServerUrl}}"]""");
        }

        Handler = new FakeServerHandler();
        Secrets = new InMemorySecretStore();
        Remembered = new RememberedUserStore(Path.Combine(_tempDirectory, "remembered-users.json"));
        Connection = new ApiConnection(Handler, Secrets, Remembered);
        RecentServers = new RecentServersStore(recentPath);
        App = new AppViewModel(Connection, RecentServers);
    }

    public FakeServerHandler Handler { get; }

    public ApiConnection Connection { get; }

    public InMemorySecretStore Secrets { get; }

    public RememberedUserStore Remembered { get; }

    public RecentServersStore RecentServers { get; }

    public AppViewModel App { get; }

    public ServerPickerViewModel Picker => App.ServerPicker;

    public LoginViewModel Login => App.Login;

    public ShellViewModel Shell => App.Shell;

    /// <summary>The <c>&lt;server&gt;/&lt;user&gt;</c> key the stores talk about in tests.</summary>
    public static string KeyFor(string user = DefaultUser) =>
        Spotledger.Client.Api.SecretKey.For(ServerUrl, user);

    /// <summary>Runs the picker's check against the fake server and waits for it to land.</summary>
    public Task CheckServerAsync()
    {
        Picker.ServerUrl = ServerUrl;
        Picker.CheckCommand.Execute(null);
        return WaitUntilAsync(() => !Picker.CheckCommand.IsRunning, "the ready check to finish");
    }

    /// <summary>The picker accepts a ready server: runs the start-up resume and waits for the stage to settle.</summary>
    public async Task OpenLoginAsync()
    {
        Picker.NextCommand.Execute(null);
        await WaitUntilAsync(() => !App.ShowServerPicker, "the stage to leave the picker");
        await App.SessionInitialization;
    }

    /// <summary>Signs in through the login form and waits for the outcome (shell stage or error).</summary>
    public async Task SignInAsync(string user = DefaultUser, string password = DefaultPassword, bool rememberMe = false)
    {
        Login.User = user;
        Login.Password = password;
        Login.RememberMe = rememberMe;
        Login.SignInCommand.Execute(null);
        await Login.SignInCommand.ExecutionTask;
        // A shell stage change precedes its network load; do not let a test dispose the
        // connection until that load and its error handling have fully completed.
        await App.SessionInitialization;
    }

    /// <summary>Signs in with "Remember me" and confirms the token is in the fake store.</summary>
    public async Task SignInRememberedAsync()
    {
        await SignInAsync(rememberMe: true);
        Assert.True(await Secrets.GetSecretAsync(KeyFor(), TestContext.Current.CancellationToken) is not null, "the remembered token was stored");
    }

    /// <summary>Waits until the shell finished its post-login session load.</summary>
    public Task ShellInitializedAsync()
    {
        return App.SessionInitialization;
    }

    public void Dispose()
    {
        Connection.Dispose();
        try
        {
            Directory.Delete(_tempDirectory, recursive: true);
        }
        catch (IOException)
        {
            // Best-effort temp cleanup; the OS temp dir is swept independently.
        }
        catch (UnauthorizedAccessException)
        {
        }
    }

    /// <summary>Polls <paramref name="condition"/> until true or the deadline passes.</summary>
    public static async Task WaitUntilAsync(Func<bool> condition, string because, int timeoutMilliseconds = 5000)
    {
        var deadline = Environment.TickCount64 + timeoutMilliseconds;
        while (!condition())
        {
            if (Environment.TickCount64 > deadline)
            {
                throw new TimeoutException($"Timed out waiting for {because}.");
            }

            await Task.Delay(15).ConfigureAwait(false);
        }
    }
}