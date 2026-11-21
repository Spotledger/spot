using Avalonia;

namespace Spotledger.Client.Desktop;

internal static class Program
{
    [STAThread]
    public static void Main(string[] args)
    {
        App.ShowTokenGallery = args.Contains("--gallery", StringComparer.Ordinal);
        var avaloniaArgs = args.Where(argument => !string.Equals(argument, "--gallery", StringComparison.Ordinal)).ToArray();
        BuildAvaloniaApp().StartWithClassicDesktopLifetime(avaloniaArgs);
    }

    private static AppBuilder BuildAvaloniaApp() => AppBuilder.Configure<App>().UsePlatformDetect();
}
