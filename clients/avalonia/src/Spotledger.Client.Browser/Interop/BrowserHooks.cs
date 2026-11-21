using System.Runtime.InteropServices.JavaScript;
using System.Runtime.Versioning;

namespace Spotledger.Client.Browser;

/// <summary>
/// The .NET side of <c>window.spotBrowser</c>. <c>main.js</c> hands these exports to
/// <c>spot-browser.mjs</c>, which owns the public names; nothing else calls them.
/// </summary>
[SupportedOSPlatform("browser")]
public static partial class BrowserHooks
{
    private static BrowserHostState? _state;
    private static bool _testSignIn;

    internal static void Attach(BrowserHostState state)
    {
        _state = state;
        if (_testSignIn)
        {
            state.EnableTestSignIn();
        }
    }

    /// <summary>
    /// Called by <c>main.js</c> before the app starts, and only when the page URL carried <c>?testSignIn=1</c>.
    /// </summary>
    [JSExport]
    public static void EnableTestSignIn()
    {
        _testSignIn = true;
        _state?.EnableTestSignIn();
    }

    /// <summary>
    /// Test only (see <see cref="BrowserHostState.SignInAsync"/>): false unless the flag was set and the URL is on
    /// the loopback host. The arguments are never logged or stored.
    /// </summary>
    [JSExport]
    public static async Task<bool> SignIn(string url, string user, string password) =>
        _state is null ? false : await _state.SignInAsync(url, user, password).ConfigureAwait(true);

    /// <summary>The metrics JSON (<see cref="BrowserMetrics.ToJson"/>).</summary>
    [JSExport]
    public static string MetricsJson() => _state?.Metrics.ToJson() ?? "{\"schema\":1,\"ready\":false,\"marksUs\":{},\"counters\":{},\"labels\":{}}";

    /// <summary>Opens a document form tab; false when the shell cannot (not signed in, not started).</summary>
    [JSExport]
    public static async Task<bool> OpenForm(string doctype, string id) =>
        _state is null ? false : await _state.OpenFormAsync(doctype, id).ConfigureAwait(true);

    /// <summary>Opens a list tab; false when the shell cannot (not signed in, not started).</summary>
    [JSExport]
    public static async Task<bool> OpenList(string doctype) =>
        _state is null ? false : await _state.OpenListAsync(doctype).ConfigureAwait(true);
}
