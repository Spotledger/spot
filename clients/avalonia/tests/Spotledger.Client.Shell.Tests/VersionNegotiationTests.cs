using System.Net;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// Version negotiation (WP-UI-02.T3 deliverable 3): GET /api/health/live's version read
/// through the generated client's AdditionalData; refusal on a major mismatch, a
/// non-blocking notice on a newer minor, and degraded capabilities on an older minor —
/// resolved once into <see cref="ServerCapabilities"/>, which the shell queries.
/// </summary>
public sealed class VersionNegotiationTests
{
    [Fact]
    public async Task AServerWithoutAVersionIsTreatedAsTheCurrentContract()
    {
        using var connection = new ApiConnection(new FakeServerHandler());
        connection.Connect(new Uri(TestHarness.ServerUrl));

        var capabilities = await connection.NegotiateVersionAsync(TestContext.Current.CancellationToken);

        Assert.False(capabilities.VersionReported);
        Assert.True(capabilities.LookupSearch, "the 0.2.0 features of a 0.2.0 peer");
        Assert.False(capabilities.PasswordChange, "the planned 0.3.0 features are off");
        Assert.Null(capabilities.Notice);
    }

    [Fact]
    public async Task AMajorMismatchIsRefusedWithTheContractWording()
    {
        var handler = new FakeServerHandler
        {
            LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"1.4.2"}"""),
        };
        using var connection = new ApiConnection(handler);
        connection.Connect(new Uri(TestHarness.ServerUrl));

        await Assert.ThrowsAsync<ApiVersionMismatchException>(() => connection.NegotiateVersionAsync(TestContext.Current.CancellationToken));

        // The picker surfaces the refusal (the brief's exact sentence) and refuses the server.
        using var h = new TestHarness();
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"1.4.2"}""");
        await h.CheckServerAsync();
        Assert.False(h.Picker.IsReady);
        Assert.Equal("This server speaks API version 1; this app needs 0.", h.Picker.StatusMessage);
    }

    [Fact]
    public async Task ANewerMinorLeavesANonBlockingNoticeAndKeepsEverythingOn()
    {
        var handler = new FakeServerHandler
        {
            LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}"""),
        };
        using var connection = new ApiConnection(handler);
        connection.Connect(new Uri(TestHarness.ServerUrl));

        var capabilities = await connection.NegotiateVersionAsync(TestContext.Current.CancellationToken);

        Assert.Equal("0.3.0", capabilities.ServerVersion);
        Assert.NotNull(capabilities.Notice);
        Assert.Contains("newer API", capabilities.Notice, StringComparison.Ordinal);
        Assert.True(capabilities.PasswordChange);
        Assert.True(capabilities.SessionManagement);
        Assert.True(capabilities.LookupSearch);
        Assert.Same(capabilities, connection.Capabilities);
    }

    [Fact]
    public async Task AnOlderMinorDisablesTheFeaturesTheChangelogAddedLater()
    {
        var handler = new FakeServerHandler
        {
            LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.1.0"}"""),
        };
        using var connection = new ApiConnection(handler);
        connection.Connect(new Uri(TestHarness.ServerUrl));

        var capabilities = await connection.NegotiateVersionAsync(TestContext.Current.CancellationToken);

        Assert.Null(capabilities.Notice);
        Assert.False(capabilities.LookupSearch, "lookup_search arrived with 0.2.0 (CHANGELOG)");
        Assert.False(capabilities.RichFieldMeta, "the complete field meta arrived with 0.2.0");
        Assert.False(capabilities.ExtendedFieldTypes, "long_text/attach/secret/table_multiselect arrived with 0.2.0");
        Assert.False(capabilities.PasswordChange);
        Assert.False(capabilities.SessionManagement);
        Assert.Same(capabilities, connection.Capabilities);
    }

    [Fact]
    public async Task TheShellShowsTheNoticeAfterThePickerCheck()
    {
        using var h = new TestHarness();
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}""");
        await h.CheckServerAsync();

        Assert.True(h.Picker.IsReady);
        Assert.NotNull(h.Picker.VersionNotice);
        Assert.True(h.Connection.Capabilities.PasswordChange);

        // The notice is non-blocking: the picker stays usable and Next proceeds.
        h.Picker.NextCommand.Execute(null);
        await h.OpenLoginAsync();
        Assert.True(h.App.ShowLogin);
    }

    [Fact]
    public async Task TheSessionDialogCommandsFollowTheCapabilities()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        Assert.False(h.Connection.Capabilities.PasswordChange);
        var shell = h.Shell;

        var change = await shell.RunAsync("shell.changePassword", CommandRoute.Menu);
        Assert.False(change.Executed);
        Assert.Contains("no password change", change.Availability.Reason, StringComparison.Ordinal);

        var sessions = await shell.RunAsync("shell.mySessions", CommandRoute.Menu);
        Assert.False(sessions.Executed);
        Assert.Contains("no session management", sessions.Availability.Reason, StringComparison.Ordinal);

        // A 0.3 server turns both on.
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}""");
        await h.CheckServerAsync();
        var changeOn = await shell.RunAsync("shell.changePassword", CommandRoute.Menu);
        Assert.True(changeOn.Executed);
        Assert.True(shell.ChangePasswordVisible);
        shell.CloseChangePassword();

        var sessionsOn = await shell.RunAsync("shell.mySessions", CommandRoute.Menu);
        Assert.True(sessionsOn.Executed);
        Assert.True(shell.MySessionsVisible);
        shell.CloseMySessions();
    }

    private sealed class FakeServerHandlerWithVersion(string version)
    {
        public string Version { get; } = version;
    }
}