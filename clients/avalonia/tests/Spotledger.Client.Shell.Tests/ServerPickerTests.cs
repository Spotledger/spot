using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// Step one of the app: enter/select a server URL, check <c>GET /api/health/ready</c>
/// against the fake server, remember ready servers, and enable Next only when ready.
/// </summary>
public sealed class ServerPickerTests
{
    private static readonly string[] SingleServer = [TestHarness.ServerUrl];

    [Fact]
    public async Task ReadyServerEnablesNextAndRemembersTheUrl()
    {
        using var h = new TestHarness();

        await h.CheckServerAsync();

        Assert.True(h.Picker.IsReady);
        Assert.Equal("The server is ready.", h.Picker.StatusMessage);
        Assert.True(h.Picker.NextCommand.CanExecute(null));
        Assert.Equal(SingleServer, h.Picker.RecentServers);
        Assert.Equal(SingleServer, h.RecentServers.Load());
    }

    [Fact]
    public async Task NextFiresReadyToLoginOnlyAfterAReadyCheck()
    {
        using var h = new TestHarness();

        Assert.False(h.Picker.NextCommand.CanExecute(null));

        await h.CheckServerAsync();
        var raised = false;
        h.App.ServerPicker.ReadyToLogin += (_, _) => raised = true;
        h.Picker.NextCommand.Execute(null);

        Assert.True(raised);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
    }

    [Fact]
    public async Task NotReadyServerShowsFailedCheckDetailAndBlocksNext()
    {
        using var h = new TestHarness();
        h.Handler.ReadyResponder = _ => FakeServerHandler.Json(
            System.Net.HttpStatusCode.OK,
            """{"status":"not_ready","checks":[{"name":"primary_connection","ok":false,"detail":"no reply"}]}""");

        await h.CheckServerAsync();

        Assert.False(h.Picker.IsReady);
        Assert.Contains("primary_connection: no reply", h.Picker.StatusMessage);
        Assert.False(h.Picker.NextCommand.CanExecute(null));
        Assert.Empty(h.RecentServers.Load());
    }

    [Fact]
    public async Task UnreachableServerShowsAnErrorMessageAndBlocksNext()
    {
        using var h = new TestHarness();
        h.Handler.ReadyResponder = _ => throw new HttpRequestException("Connection refused");

        await h.CheckServerAsync();

        Assert.False(h.Picker.IsReady);
        Assert.StartsWith("Could not reach the server", h.Picker.StatusMessage);
        Assert.False(h.Picker.NextCommand.CanExecute(null));
    }

    [Fact]
    public void NonHttpUrlIsRejectedWithoutARequest()
    {
        using var h = new TestHarness();
        h.Picker.ServerUrl = "ftp://nope.example";

        Assert.False(h.Picker.CheckCommand.CanExecute(null));
        Assert.Empty(h.Handler.Requests);
    }

    [Fact]
    public void SelectingARecentServerSetsTheUrl()
    {
        using var h = new TestHarness(seedRecentFile: true);

        h.Picker.SelectRecentServer(TestHarness.ServerUrl);

        Assert.Equal(TestHarness.ServerUrl, h.Picker.ServerUrl);
    }

    [Fact]
    public async Task EditingTheUrlInvalidatesThePreviousReadyVerdict()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        Assert.True(h.Picker.IsReady);

        h.Picker.ServerUrl = "http://other.example";

        Assert.False(h.Picker.IsReady);
        Assert.False(h.Picker.NextCommand.CanExecute(null));
    }

    [Fact]
    public async Task RecentServersLoadFromDiskAndStayUnique()
    {
        using var h = new TestHarness(seedRecentFile: true);

        Assert.Equal(SingleServer, h.Picker.RecentServers);

        await h.CheckServerAsync();

        // Re-checking the same server keeps exactly one entry (no duplicates).
        Assert.Equal(SingleServer, h.RecentServers.Load());
    }
}
