using Avalonia;

namespace Spotledger.AdminCenter.App;

internal static class Program
{
    [STAThread]
    public static void Main(string[] args) =>
        AppBuilder.Configure<AdminApp>().UsePlatformDetect().StartWithClassicDesktopLifetime(args);
}
