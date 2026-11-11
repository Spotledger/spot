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
}
