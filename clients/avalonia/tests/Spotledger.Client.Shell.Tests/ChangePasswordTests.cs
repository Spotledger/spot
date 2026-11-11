using System.Net;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The change-password dialog and My sessions (WP-UI-02.T3 deliverable 4) against the
/// fake server: policy violations as per-field messages under their fields, the passwords
/// dropped after every outcome, and session ending that removes exactly the chosen row.
/// </summary>
public sealed class ChangePasswordTests
{
    [Fact]
    public async Task ChangePasswordSendsBothFieldsAndNeverStoresThem()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}""");
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        await h.Shell.OpenChangePasswordAsync();
        var dialog = h.Shell.ChangePassword!;
        dialog.CurrentPassword = "old-passphrase-01";
        dialog.NewPassword = "new-passphrase-02";
        dialog.SaveCommand.Execute(null);
        await dialog.SaveCommand.ExecutionTask;

        Assert.False(h.Shell.ChangePasswordVisible, "the dialog closed on success");
        var request = h.Handler.RequestsFor("/api/v1/auth/password").Single();
        Assert.Equal("POST", request.Method);
        Assert.Contains("current_password", request.Body, StringComparison.Ordinal);
        Assert.Contains("old-passphrase-01", request.Body, StringComparison.Ordinal);
        Assert.Contains("new-passphrase-02", request.Body, StringComparison.Ordinal);
        // The 401 machinery was not involved (a 204, not a prompt); no secret store write.
        Assert.DoesNotContain(h.Secrets.Writes, write => write.Value.Contains("passphrase", StringComparison.Ordinal));
    }

    [Fact]
    public async Task PolicyViolationsLandUnderTheirFields()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}""");
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        await h.Shell.OpenChangePasswordAsync();
        var dialog = h.Shell.ChangePassword!;
        h.Handler.PasswordResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.UnprocessableEntity,
            "validation.failed",
            "The new password does not meet the policy.",
            """[{"field":"new_password","code":"auth.password_too_short","message":"Use at least 12 characters."},{"field":"current_password","code":"auth.wrong_password","message":"The current password is wrong."}]""");
        dialog.CurrentPassword = "wrong-current";
        dialog.NewPassword = "short";
        dialog.SaveCommand.Execute(null);
        await dialog.SaveCommand.ExecutionTask;

        Assert.Equal("The new password does not meet the policy.", dialog.FormMessage);
        Assert.Equal("The current password is wrong.", dialog.CurrentFieldMessage);
        Assert.Equal("Use at least 12 characters.", dialog.NewFieldMessage);
        // The dialog stays open with the passwords cleared for a retype.
        Assert.True(h.Shell.ChangePasswordVisible);
        Assert.Equal(string.Empty, dialog.CurrentPassword);
        Assert.Equal(string.Empty, dialog.NewPassword);

        h.Handler.PasswordResponder = _ => new HttpResponseMessage(HttpStatusCode.NoContent);
        dialog.CurrentPassword = "old-passphrase-01";
        dialog.NewPassword = "new-passphrase-02";
        dialog.SaveCommand.Execute(null);
        await dialog.SaveCommand.ExecutionTask;
        Assert.False(h.Shell.ChangePasswordVisible);
    }

    [Fact]
    public async Task CancelJustClosesWithoutACall()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}""");
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();
        await h.Shell.OpenChangePasswordAsync();
        var dialog = h.Shell.ChangePassword!;
        dialog.CurrentPassword = "typed";
        dialog.NewPassword = "typed";

        dialog.CancelCommand.Execute(null);

        Assert.False(h.Shell.ChangePasswordVisible);
        Assert.Empty(h.Handler.RequestsFor("/api/v1/auth/password"));
        Assert.Equal(string.Empty, dialog.CurrentPassword);
        Assert.Equal(string.Empty, dialog.NewPassword);
    }
}

/// <summary>My sessions: listing gates on the capability; ending removes exactly one row.</summary>
public sealed class MySessionsTests
{
    [Fact]
    public async Task ListAndEndASessionThroughTheFake()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}""");
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        await h.Shell.OpenMySessionsAsync();
        var dialog = h.Shell.MySessions!;
        Assert.True(dialog.IsLoaded);
        Assert.Equal(2, dialog.Sessions.Count);
        Assert.True(dialog.Sessions[0].Current, "the fake marks sess-1 as this session");

        // The current session cannot be ended through the dialog (Sign out covers it).
        Assert.False(dialog.EndSessionCommand.CanExecute(dialog.Sessions[0]));
        Assert.True(dialog.EndSessionCommand.CanExecute(dialog.Sessions[1]));

        dialog.EndSessionCommand.Execute(dialog.Sessions[1]);
        await TestHarness.WaitUntilAsync(() => dialog.Sessions.Count == 1, "the ended session to leave the list");

        var delete = h.Handler.Requests.Single(request =>
            request.Method == "DELETE" && request.Path.StartsWith("/api/v1/auth/sessions/", StringComparison.Ordinal));
        Assert.EndsWith("sess-2", delete.Path, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ALoadFailureShowsInTheDialog()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Handler.LiveResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """{"status":"live","version":"0.3.0"}""");
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        h.Handler.SessionsListResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Forbidden, "permission.denied", "You may not list sessions.");
        await h.Shell.OpenMySessionsAsync();

        Assert.Equal("You may not list sessions.", h.Shell.MySessions!.LoadError);
        Assert.False(h.Shell.MySessions.IsLoaded);
    }
}