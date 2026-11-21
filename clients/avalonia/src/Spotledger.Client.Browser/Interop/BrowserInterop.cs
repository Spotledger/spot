using System.Runtime.InteropServices.JavaScript;
using System.Runtime.Versioning;

namespace Spotledger.Client.Browser;

/// <summary>The imports from <c>wwwroot/spot-browser.mjs</c> (module name <c>spotBrowser</c>).</summary>
[SupportedOSPlatform("browser")]
internal static partial class SpotBrowserJs
{
    internal const string Module = "spotBrowser";

    [JSImport("signalReady", Module)]
    internal static partial void SignalReady();

    [JSImport("nowMicros", Module)]
    [return: JSMarshalAs<JSType.BigInt>]
    internal static partial long NowMicros();

    [JSImport("storageAvailable", Module)]
    internal static partial bool StorageAvailable();

    [JSImport("storageGet", Module)]
    internal static partial string? StorageGet(string key);

    [JSImport("storageSet", Module)]
    internal static partial void StorageSet(string key, string value);

    [JSImport("storageRemove", Module)]
    internal static partial void StorageRemove(string key);
}

/// <summary>The imports from <c>wwwroot/calc.mjs</c> (module name <c>spotCalc</c>).</summary>
[SupportedOSPlatform("browser")]
internal static partial class SpotCalcJs
{
    internal const string Module = "spotCalc";

    [JSImport("init", Module)]
    internal static partial Task<bool> InitAsync(string url);

    [JSImport("loaded", Module)]
    internal static partial bool Loaded();

    [JSImport("evaluate", Module)]
    internal static partial string Evaluate(string requestJson);
}

/// <summary>The browser's <c>localStorage</c> as an <see cref="IBrowserStorage"/>; unavailable storage reads empty.</summary>
[SupportedOSPlatform("browser")]
internal sealed class LocalBrowserStorage : IBrowserStorage
{
    private readonly bool _available = SpotBrowserJs.StorageAvailable();

    public bool IsAvailable => _available;

    public string? Read(string key) => _available ? SpotBrowserJs.StorageGet(key) : null;

    public void Write(string key, string value)
    {
        if (_available)
        {
            SpotBrowserJs.StorageSet(key, value);
        }
    }

    public void Delete(string key)
    {
        if (_available)
        {
            SpotBrowserJs.StorageRemove(key);
        }
    }
}

/// <summary>The calculation module behind <c>calc.mjs</c>.</summary>
[SupportedOSPlatform("browser")]
internal sealed class JsWasmCalcModule : IWasmCalcModule
{
    public bool IsLoaded => SpotCalcJs.Loaded();

    public string Evaluate(string requestJson)
    {
        try
        {
            return SpotCalcJs.Evaluate(requestJson);
        }
        catch (JSException exception)
        {
            throw new WasmCalcUnavailableException("the calculation module failed", exception);
        }
    }

    public static Task<bool> LoadAsync(string url) => SpotCalcJs.InitAsync(url);
}
