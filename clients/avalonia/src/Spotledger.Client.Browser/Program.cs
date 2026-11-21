using System.Runtime.InteropServices.JavaScript;
using System.Runtime.Versioning;
using Avalonia;
using Avalonia.Browser;

[assembly: SupportedOSPlatform("browser")]

namespace Spotledger.Client.Browser;

internal static class Program
{
    private static async Task Main()
    {
        // The two JavaScript modules the head needs; neither fetches anything else.
        await JSHost.ImportAsync(SpotBrowserJs.Module, "../spot-browser.mjs");
        await JSHost.ImportAsync(SpotCalcJs.Module, "../calc.mjs");

        // Time is an argument: the monotonic clock of the page, never the wall clock.
        var clock = new Func<long>(() => SpotBrowserJs.NowMicros());
        var state = BrowserHostState.Compose(
            new LocalBrowserStorage(),
            clock,
            Path.Combine(Path.GetTempPath(), "spotledger", "recent-servers.json"));
        BrowserHooks.Attach(state);
        BrowserApp.State = state;

        await AppBuilder.Configure<BrowserApp>().StartBrowserAppAsync("out");
    }
}
