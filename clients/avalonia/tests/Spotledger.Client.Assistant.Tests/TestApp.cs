using Avalonia;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Themes.Fluent;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Assistant.Tests.TestAppFactory))]
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Assistant.Tests;

/// <summary>The headless test application.</summary>
public sealed class TestApp : Application
{
    public override void Initialize() => Styles.Add(new FluentTheme());
}

/// <summary>Builds the headless app.</summary>
public static class TestAppFactory
{
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<TestApp>().UseHeadless(new AvaloniaHeadlessPlatformOptions());
}
