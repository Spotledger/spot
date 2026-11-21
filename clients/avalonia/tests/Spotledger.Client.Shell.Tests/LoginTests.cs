using System.Net;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// Step two of the app: POST /api/v1/auth/login against the fake server. Success lands on
/// the shell with the session user; contract errors show as form/field messages; an
/// auth.totp_required answer reveals the code field and a retry succeeds.
/// </summary>
public sealed class LoginTests
{
    private const string TotpErrorJson =
        """{"error":{"code":"auth.totp_required","message":"The user has two-factor authentication; repeat with a one-time code.","retryable":false,"request_id":"req-1"}}""";

    [Fact]
    public async Task LoginSuccessMovesToShellAndShowsSessionUser()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);

        await h.SignInAsync();
        await h.ShellInitializedAsync();

        Assert.Equal(AppStage.Shell, h.App.CurrentStage);
        Assert.True(h.App.ShowShell);
        Assert.Equal("Ayesha Khan", h.Shell.UserName);
        Assert.Equal("ayesha@acme.test", h.Shell.UserEmail);
        Assert.Equal("acme.test", h.Shell.SiteName);
        Assert.Equal("2026-09-30T08:00:00Z", h.Shell.ExpiresAt);
    }

    [Fact]
    public async Task LoginSuccessDropsThePasswordAndKeepsTheTokenInMemoryOnly()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);

        await h.SignInAsync();

        Assert.Equal(string.Empty, h.Login.Password);
        Assert.True(h.Connection.Tokens.HasToken);
        // The session token goes nowhere near the recent-servers file.
        Assert.DoesNotContain(TestHarness.SessionToken, string.Join('\n', h.RecentServers.Load()));
    }

    [Fact]
    public async Task BadPasswordShowsTheContractErrorOnTheForm()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        h.Handler.LoginResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized,
            "auth.invalid_credentials",
            "The credentials are wrong or no longer valid.");

        await h.SignInAsync();

        Assert.Equal(AppStage.Login, h.App.CurrentStage);
        Assert.Equal("The credentials are wrong or no longer valid.", h.Login.FormMessage);
        Assert.False(h.Connection.Tokens.HasToken);
        Assert.Equal(string.Empty, h.Login.Password);
    }

    [Fact]
    public async Task FieldErrorsLandOnTheirFields()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        h.Handler.LoginResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.UnprocessableEntity,
            "validation.failed",
            "The sign-in form is incomplete.",
            """[{"field":"user","code":"validation.required","message":"A user is required."},{"field":"password","code":"validation.required","message":"A password is required."}]""");

        await h.SignInAsync();

        Assert.Equal("A user is required.", h.Login.UserFieldMessage);
        Assert.Equal("A password is required.", h.Login.PasswordFieldMessage);
        Assert.Equal("The sign-in form is incomplete.", h.Login.FormMessage);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
    }

    [Fact]
    public async Task TotpRequiredRevealsCodeFieldAndRetrySucceeds()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);

        var attempts = 0;
        h.Handler.LoginResponder = _ =>
        {
            attempts++;
            return attempts == 1
                ? FakeServerHandler.Json(HttpStatusCode.Unauthorized, TotpErrorJson)
                : FakeServerHandler.Json(
                    HttpStatusCode.OK,
                    """{"data":{"token":"3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs","expires_at":"2026-09-30T08:00:00Z","user":{"id":"0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f","email":"ayesha@acme.test","full_name":"Ayesha Khan"}}}""");
        };

        await h.SignInAsync();

        Assert.True(h.Login.NeedsTotp);
        Assert.True(h.Login.ShowTotpField);
        Assert.Equal("The user has two-factor authentication; repeat with a one-time code.", h.Login.FormMessage);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
        Assert.False(h.Connection.Tokens.HasToken);

        h.Login.TotpCode = "123456";
        await h.SignInAsync();

        Assert.True(h.Connection.Tokens.HasToken);
        Assert.Equal(string.Empty, h.Login.TotpCode);
        Assert.False(h.Login.NeedsTotp);
        var lastLogin = h.Handler.RequestsFor("/api/v1/auth/login").Last();
        Assert.Contains("\"totp\":\"123456\"", lastLogin.Body, StringComparison.Ordinal);
    }

    [Fact]
    public async Task BackCommandReturnsToServerPicker()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);

        h.Login.BackCommand.Execute(null);

        Assert.Equal(AppStage.ServerPicker, h.App.CurrentStage);
    }

    [Fact]
    public void SignInCommandRequiresUserAndPassword()
    {
        using var h = new TestHarness();

        Assert.False(h.Login.SignInCommand.CanExecute(null));
        h.Login.User = "ayesha@acme.test";
        Assert.False(h.Login.SignInCommand.CanExecute(null));
        h.Login.Password = "x";
        Assert.True(h.Login.SignInCommand.CanExecute(null));
    }
}
