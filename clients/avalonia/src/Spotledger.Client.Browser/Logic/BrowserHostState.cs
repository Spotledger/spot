namespace Spotledger.Client.Browser;

/// <summary>
/// What the page-level hooks (<c>window.spotBrowser</c>) can ask the app to do. The head implements it
/// over the shell; the tests implement it with a recorder. Opening is asynchronous because the shell
/// must be on screen first.
/// </summary>
public interface IWorkspaceOpener
{
    /// <summary>Opens a document form tab. Returns false when it cannot (not signed in).</summary>
    Task<bool> OpenFormAsync(string doctype, string id);

    /// <summary>Opens a list tab. Returns false when it cannot (not signed in).</summary>
    Task<bool> OpenListAsync(string doctype);

    /// <summary>
    /// Test-only: points the shell at <paramref name="url"/> and signs in through the same server picker and
    /// login view models (so the same <c>ApiConnection.LoginAsync</c>) the screens use. True when the shell
    /// then holds a session. The password is never stored or logged.
    /// </summary>
    Task<bool> SignInAsync(string url, string user, string password);
}

/// <summary>
/// The composed state of the browser head, built before Avalonia starts: the metrics, the calculation
/// path, and the recent-server bridge over the browser storage. Building it touches no UI and loads no
/// module bundle, which is what keeps the first-paint path small: the shell (server picker, login,
/// home) is the only thing started behind it. The rest of the wiring (the Avalonia application and
/// the JavaScript hooks) is in the files that need the browser.
/// </summary>
public sealed class BrowserHostState
{
    private IWorkspaceOpener? _opener;
    private bool _testSignInEnabled;

    private BrowserHostState(BrowserMetrics metrics, CalculationPath calculation, RecentServersBridge recentServers, IBrowserStorage storage)
    {
        Metrics = metrics;
        Calculation = calculation;
        RecentServers = recentServers;
        Storage = storage;
    }

    /// <summary>The collector read by <c>window.spotBrowser.metrics()</c>.</summary>
    public BrowserMetrics Metrics { get; }

    /// <summary>The calculation path (managed until a module loads).</summary>
    public CalculationPath Calculation { get; }

    /// <summary>The recent-server bridge; the shell's store is built on its file path.</summary>
    public RecentServersBridge RecentServers { get; }

    /// <summary>The browser storage (possibly the unavailable one).</summary>
    public IBrowserStorage Storage { get; }

    /// <summary>True when the page was loaded with <c>?testSignIn=1</c> (set once by <c>main.js</c>); false in every real deployment.</summary>
    public bool TestSignInEnabled => _testSignInEnabled;

    /// <summary>The shell's opener, set once the shell exists; null before.</summary>
    public IWorkspaceOpener? Opener => _opener;

    /// <summary>
    /// Builds the state: marks the start of .NET code, hydrates the recent-server file from the storage
    /// and records which calculation backend and storage are active.
    /// </summary>
    /// <param name="storage">The browser storage (a fake in tests).</param>
    /// <param name="nowMicros">The monotonic clock in microseconds (a fake in tests).</param>
    /// <param name="recentServersFile">The file the shell's recent-server store uses.</param>
    public static BrowserHostState Compose(IBrowserStorage storage, Func<long> nowMicros, string recentServersFile)
    {
        ArgumentNullException.ThrowIfNull(storage);
        ArgumentNullException.ThrowIfNull(nowMicros);
        ArgumentException.ThrowIfNullOrWhiteSpace(recentServersFile);
        var metrics = new BrowserMetrics(nowMicros);
        metrics.Mark("dotnetStart");
        var bridge = new RecentServersBridge(storage, recentServersFile);
        bridge.Hydrate();
        var calculation = new CalculationPath();
        var state = new BrowserHostState(metrics, calculation, bridge, storage);
        metrics.Label("calc", calculation.ActiveName);
        metrics.Label("storage", storage.IsAvailable ? "available" : "unavailable");
        calculation.ActiveChanged += (_, _) =>
        {
            metrics.Label("calc", calculation.ActiveName);
            metrics.Mark("calcSwitched");
        };
        return state;
    }

    /// <summary>Records that the first frame of the shell was shown (the server picker or login).</summary>
    public void MarkFirstPaint() => Metrics.Mark("firstPaint");

    /// <summary>Records that the shell is interactive; <c>window.spotBrowser.ready</c> resolves after this.</summary>
    public void MarkReady() => Metrics.MarkReady();

    /// <summary>Turns the test sign-in hook on. Called at most once, at start-up, when the page URL carried the flag.</summary>
    public void EnableTestSignIn() => _testSignInEnabled = true;

    /// <summary>Gives the hooks the shell's opener.</summary>
    public void AttachOpener(IWorkspaceOpener opener)
    {
        ArgumentNullException.ThrowIfNull(opener);
        _opener = opener;
    }

    /// <summary>The <c>openForm</c> hook: counted, then handed to the shell. False before the shell exists.</summary>
    public async Task<bool> OpenFormAsync(string doctype, string id)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        ArgumentNullException.ThrowIfNull(id);
        if (_opener is null)
        {
            return false;
        }

        Metrics.Count("openForm");
        var opened = await _opener.OpenFormAsync(doctype, id).ConfigureAwait(true);
        Metrics.Mark(opened ? "lastFormOpened" : "lastFormRefused");
        return opened;
    }

    /// <summary>The <c>openList</c> hook: counted, then handed to the shell. False before the shell exists.</summary>
    public async Task<bool> OpenListAsync(string doctype)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        if (_opener is null)
        {
            return false;
        }

        Metrics.Count("openList");
        var opened = await _opener.OpenListAsync(doctype).ConfigureAwait(true);
        Metrics.Mark(opened ? "lastListOpened" : "lastListRefused");
        return opened;
    }

    /// <summary>
    /// True only for an absolute http(s) URL whose host is exactly <c>127.0.0.1</c>, <c>localhost</c> or
    /// <c>[::1]</c> and that carries no user-info. Anything else (a look-alike host, another scheme, a
    /// relative URL) is refused.
    /// </summary>
    public static bool IsLoopbackUrl(string? url)
    {
        if (string.IsNullOrWhiteSpace(url) || !Uri.TryCreate(url.Trim(), UriKind.Absolute, out var uri))
        {
            return false;
        }

        if (uri.Scheme != Uri.UriSchemeHttp && uri.Scheme != Uri.UriSchemeHttps)
        {
            return false;
        }

        if (!string.IsNullOrEmpty(uri.UserInfo))
        {
            return false;
        }

        return uri.Host is "127.0.0.1" or "localhost" or "[::1]" or "::1";
    }

    /// <summary>
    /// The <c>signIn</c> hook (test only): refused (false, nothing called) unless the test flag is on, the shell
    /// exists, the URL is on the loopback host and both user and password are present. The password is handed
    /// to the opener and nowhere else: no metric, label or log line carries it.
    /// </summary>
    public async Task<bool> SignInAsync(string url, string user, string password)
    {
        if (!_testSignInEnabled || _opener is null || !IsLoopbackUrl(url)
            || string.IsNullOrWhiteSpace(user) || string.IsNullOrEmpty(password))
        {
            return false;
        }

        Metrics.Count("signIn");
        bool signedIn;
        try
        {
            signedIn = await _opener.SignInAsync(url.Trim(), user, password).ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            // The exception text is dropped on purpose: it is not allowed to carry the arguments out.
            signedIn = false;
        }

        Metrics.Mark(signedIn ? "lastSignedIn" : "lastSignInRefused");
        return signedIn;
    }
}
