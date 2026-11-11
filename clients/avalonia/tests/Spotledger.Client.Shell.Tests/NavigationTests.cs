using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The app's stage machine (server picker → login → shell), the bearer header lifecycle,
/// logout in all its shapes, and the session-expired 401 that lands the user back on login
/// with the tabs cleared. All against the fake server.
/// </summary>
public sealed class NavigationTests
{
    [Fact]
    public void AppStartsOnTheServerPicker()
    {
        using var h = new TestHarness();

        Assert.Equal(AppStage.ServerPicker, h.App.CurrentStage);
        Assert.True(h.App.ShowServerPicker);
        Assert.False(h.App.ShowLogin);
        Assert.False(h.App.ShowShell);
    }

    [Fact]
    public async Task LoginAbsentsBearerAndAuthorizedCallsCarryIt()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();

        // Health check runs before login: no Authorization header on any request yet.
        Assert.All(h.Handler.Requests, request => Assert.Null(request.Authorization));

        await h.SignInAsync();
        await h.ShellInitializedAsync();

        var login = h.Handler.RequestsFor("/api/v1/auth/login").Single();
        Assert.Null(login.Authorization);

        var session = h.Handler.RequestsFor("/api/v1/auth/session").Single();
        Assert.Equal($"Bearer {TestHarness.SessionToken}", session.Authorization);
    }

    [Fact]
    public async Task LogoutCallsTheEndpointClearsTabsAndReturnsToLogin()
    {
        using var h = await ShellWithTabsAsync();

        h.Shell.OpenWorkspaceCommand.Execute("Selling");
        await TestHarness.WaitUntilAsync(() => h.Shell.Tabs.Count == 2, "the Selling tab to open");

        await LogoutAsync(h);

        var logout = h.Handler.RequestsFor("/api/v1/auth/logout").Single();
        Assert.Equal("POST", logout.Method);
        Assert.False(h.Connection.Tokens.HasToken);
        Assert.Empty(h.Shell.Tabs);
        Assert.Null(h.Shell.SelectedTab);
        Assert.Equal(string.Empty, h.Shell.UserName);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
        Assert.True(h.App.ShowLogin);
    }

    [Fact]
    public async Task LogoutReturnsToLoginEvenWhenTheServerCallFails()
    {
        using var h = await ShellWithTabsAsync();
        h.Handler.LogoutResponder = _ => throw new HttpRequestException("Connection reset");

        await LogoutAsync(h);

        Assert.NotEmpty(h.Handler.RequestsFor("/api/v1/auth/logout"));
        Assert.False(h.Connection.Tokens.HasToken);
        Assert.Empty(h.Shell.Tabs);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
    }

    [Fact]
    public async Task SessionExpired401ReturnsToLoginWithTabsCleared()
    {
        using var h = await ShellWithTabsAsync();
        h.Handler.SessionExpiredCode = "auth.session_expired";

        // Any authorized call answering 401 auth.session_expired triggers it; the session
        // reload is the next such call.
        await h.Shell.InitializeAsync(TestContext.Current.CancellationToken);
        await TestHarness.WaitUntilAsync(
            () => h.App.CurrentStage == AppStage.Login,
            "the 401 to land the app back on login");

        Assert.Empty(h.Shell.Tabs);
        Assert.Null(h.Shell.SelectedTab);
        Assert.False(h.Connection.Tokens.HasToken);
        Assert.True(h.App.ShowLogin);
    }

    [Fact]
    public async Task InvalidCredentials401AlsoBreaksTheSession()
    {
        using var h = await ShellWithTabsAsync();
        h.Handler.SessionExpiredCode = "auth.invalid_credentials";

        await h.Shell.InitializeAsync(TestContext.Current.CancellationToken);
        await TestHarness.WaitUntilAsync(
            () => h.App.CurrentStage == AppStage.Login,
            "the 401 to land the app back on login");

        Assert.Empty(h.Shell.Tabs);
        Assert.False(h.Connection.Tokens.HasToken);
    }

    [Fact]
    public async Task FullWalkthroughPickerLoginShellLogout()
    {
        using var h = new TestHarness();

        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);

        await h.SignInAsync();
        await h.ShellInitializedAsync();
        Assert.Equal(AppStage.Shell, h.App.CurrentStage);
        Assert.Equal("Ayesha Khan", h.Shell.UserName);
        Assert.Equal("acme.test", h.Shell.SiteName);

        await LogoutAsync(h);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
    }

    private static async Task LogoutAsync(TestHarness h)
    {
        h.Shell.LogoutCommand.Execute(null);
        await TestHarness.WaitUntilAsync(
            () => h.App.CurrentStage == AppStage.Login,
            "logout to return to login");
    }

    /// <summary>Walks picker → login → shell and waits for the Home tab to be open.</summary>
    private static async Task<TestHarness> ShellWithTabsAsync()
    {
        var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();
        Assert.Equal(AppStage.Shell, h.App.CurrentStage);
        Assert.Single(h.Shell.Tabs);
        return h;
    }
}
