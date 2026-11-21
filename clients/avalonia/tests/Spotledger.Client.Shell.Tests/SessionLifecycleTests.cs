using System.Net;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The production-grade session (WP-UI-02.T3): start-up resume from the OS secret store,
/// the expired-token start, the mid-session 401 with concurrent calls (one event, one
/// prompt, one retry each, tabs preserved), the cancelled prompt and the read-only state,
/// and the rule that the secret store never sees a password.
/// </summary>
public sealed class SessionLifecycleTests
{
    [Fact]
    public async Task ResumedSessionGoesStraightToTheShell()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInRememberedAsync();
        await h.ShellInitializedAsync();
        h.Connection.Tokens.ClearToken(); // simulate the next app start (nothing in memory)

        var resume = await h.Connection.ResumeSessionAsync(TestContext.Current.CancellationToken);

        Assert.Equal(SessionResumeKind.Resumed, resume.Kind);
        Assert.True(h.Connection.Tokens.HasToken);
        Assert.Equal("Ayesha Khan", resume.Session!.User!.FullName);
        Assert.NotEmpty(h.Handler.RequestsFor("/api/v1/auth/session"));
    }

    [Fact]
    public async Task ExpiredStoredTokenIsDeletedAndTheSignInStepPrefills()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInRememberedAsync();
        await h.ShellInitializedAsync();
        h.Connection.Tokens.ClearToken();
        h.Handler.SessionResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");

        var resume = await h.Connection.ResumeSessionAsync(TestContext.Current.CancellationToken);

        Assert.Equal(SessionResumeKind.Rejected, resume.Kind);
        Assert.Equal(TestHarness.DefaultUser, resume.User);
        Assert.False(h.Connection.Tokens.HasToken);
        Assert.Null(await h.Secrets.GetSecretAsync(TestHarness.KeyFor(), TestContext.Current.CancellationToken));
        // The sign-in step prefills the server and the user (the brief's wording).
        h.Login.PrefillFromResume(h.Picker.ServerUrl, resume.User!);
        Assert.Equal(TestHarness.ServerUrl, h.Login.ServerDisplay);
        Assert.Equal(TestHarness.DefaultUser, h.Login.User);
        Assert.Equal(string.Empty, h.Login.Password);
    }

    [Fact]
    public async Task NoRememberedUserShowsThePlainSignInStep()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();

        var resume = await h.Connection.ResumeSessionAsync(TestContext.Current.CancellationToken);

        Assert.Equal(SessionResumeKind.None, resume.Kind);
        Assert.False(h.Connection.Tokens.HasToken);
    }

    [Fact]
    public async Task UnreachableServerDuringResumeKeepsTheStoredToken()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInRememberedAsync();
        await h.ShellInitializedAsync();
        h.Connection.Tokens.ClearToken();
        h.Handler.SessionResponder = _ => throw new HttpRequestException("Connection reset");

        var resume = await h.Connection.ResumeSessionAsync(TestContext.Current.CancellationToken);

        Assert.Equal(SessionResumeKind.Failed, resume.Kind);
        // The stored token survives: the outage should not cost the user their session.
        Assert.NotEmpty(await h.Secrets.GetSecretAsync(TestHarness.KeyFor(), TestContext.Current.CancellationToken) is { } value ? [value] : Array.Empty<string>());
        Assert.False(h.Connection.Tokens.HasToken);
    }

    [Fact]
    public async Task TheSecretStoreNeverReceivesAPassword()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInRememberedAsync();
        await h.ShellInitializedAsync();

        // Only the contract's token value was ever written; the password is absent.
        Assert.NotEmpty(h.Secrets.Writes);
        Assert.All(h.Secrets.Writes, write =>
        {
            Assert.Equal(TestHarness.KeyFor(), write.Key);
            Assert.NotEqual(TestHarness.DefaultPassword, write.Value);
        });
        Assert.All(h.Secrets.Writes, write =>
            Assert.Equal(TestHarness.SessionToken, write.Value));
    }

    [Fact]
    public async Task SignOutDeletesTheStoredTokenAndTheHint()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInRememberedAsync();
        await h.ShellInitializedAsync();

        h.Shell.LogoutCommand.Execute(null);
        await TestHarness.WaitUntilAsync(() => h.App.ShowLogin, "logout to return to login");

        Assert.Null(await h.Secrets.GetSecretAsync(TestHarness.KeyFor(), TestContext.Current.CancellationToken));
        Assert.Contains(TestHarness.KeyFor(), h.Secrets.Deletes);
        Assert.Null(h.Connection.Remembered.Load(TestHarness.ServerUrl));
    }

    [Fact]
    public async Task MidSession401PromptsOnceRetriesEachCallOnceAndKeepsTheTabs()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();
        await h.Shell.OpenTabAsync("Buying", TestContext.Current.CancellationToken);
        await h.Shell.OpenTabAsync("Stock", TestContext.Current.CancellationToken);
        Assert.Equal(3, h.Shell.Tabs.Count);

        var providerCalls = 0;
        var providerGate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Connection.ReSignInProvider = () =>
        {
            Interlocked.Increment(ref providerCalls);
            // Releasing the gate is "the user signed in again": a fresh token. That single
            // write is the whole change of session — the fake answers 200 to the fresh token
            // and 401 to anything else (SessionAnsweringOnlyWith), so there is no second
            // write to order and no window in which a request stamped with the new token
            // could still be answered with an expiry.
            return providerGate.Task.ContinueWith(
                _ =>
                {
                    h.Connection.Tokens.SetToken(TestHarness.RefreshedSessionToken);
                    return true;
                },
                TaskScheduler.Default);
        };
        var expiryEvents = 0;
        h.Connection.SessionExpired += (_, _) => Interlocked.Increment(ref expiryEvents);

        // Until the re-sign-in installs the fresh token every session GET expires, exactly as
        // the old hard-coded 401 responder did.
        h.Handler.SessionResponder = TestHarness.SessionAnsweringOnlyWith(TestHarness.RefreshedSessionToken);
        // The two calls are made together (that is the "two calls at once" premise), but the
        // order the server ANSWERS them in is what the prompt machinery is sensitive to: a
        // 401 that is handled while the prompt is closing can open a second prompt (the
        // generation check happens before the awaited gate, so a 401 from the same batch
        // still sees the old generation when it is released behind the prompt's teardown).
        // The test therefore pins the answer order instead of leaving it to the pool: the
        // second of the calls' requests is answered only once the prompt has closed, which is
        // the product's own "a stale 401 gets its one retry and no second prompt" path.
        var promptClosed = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var sessionArrivals = 0;
        h.Handler.BeforeResponseAsync = (request, _) =>
            string.Equals(request.RequestUri!.AbsolutePath, "/api/v1/auth/session", StringComparison.Ordinal)
                && Interlocked.Increment(ref sessionArrivals) == 2
                ? promptClosed.Task
                : Task.CompletedTask;
        // Everything before this point is awaited: the start-up sign-in, the shell's
        // post-login load (App.SessionInitialization) and the two tab opens. The tabs make
        // no session GET of their own (ShellViewModel.OpenTabAsync builds the tab locally),
        // so the baseline is a quiescent count and the two calls below are the only requests
        // that can be counted against it.
        var baseline = h.Handler.RequestsFor("/api/v1/auth/session").Count();

        var callA = h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken);
        var callB = h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken);
        // The prompt opens on the FIRST delivered 401 and blocks on the provider gate the
        // test holds. The bound is generous on purpose: this is an event-based wait that must
        // succeed, so a loaded machine must not turn it into a failure (the default 5 s was
        // the short deadline in the old form).
        await TestHarness.WaitUntilAsync(
            () => h.Connection.IsReSignInActive
                && Volatile.Read(ref providerCalls) == 1
                && Volatile.Read(ref expiryEvents) == 1
                && h.Handler.RequestsFor("/api/v1/auth/session").Count() >= baseline + 1,
            "the shared prompt to open",
            timeoutMilliseconds: 30000);
        Assert.Equal(1, providerCalls);
        Assert.Equal(1, expiryEvents);
        providerGate.SetResult();
        // The prompt's own completion closes it; only then is the held request answered, so the
        // second call cannot reach the prompt while it is being torn down.
        await TestHarness.WaitUntilAsync(
            () => !h.Connection.IsReSignInActive,
            "the prompt to close",
            timeoutMilliseconds: 30000);
        promptClosed.SetResult();

        var results = await Task.WhenAll(callA, callB);
        Assert.All(results, result => Assert.Equal("Ayesha Khan", result.User!.FullName));

        // One prompt, one event, one retry per call: the first call's 401 and its retry, and
        // for the second call either (401 + retry) or just the one request made after the
        // prompt — and no second prompt in between.
        Assert.Equal(1, providerCalls);
        Assert.Equal(1, expiryEvents);
        // What the product guarantees, not the order the requests arrived in: the three or
        // four requests of the two calls can be recorded in any interleaving, so the
        // assertions key on the token each request carried.
        var made = h.Handler.RequestsFor("/api/v1/auth/session").Skip(baseline).ToArray();
        Assert.InRange(made.Length, 3, 4);
        var fresh = TestHarness.BearerFor(TestHarness.RefreshedSessionToken);
        var withFreshToken = made
            .Where(request => string.Equals(request.Authorization, fresh, StringComparison.Ordinal))
            .ToArray();
        // Exactly one request per call carried the fresh token — the exactly-once retry (and,
        // when the second call only sent after the prompt, its single direct request) — and
        // only those were answered 200.
        Assert.Equal(2, withFreshToken.Length);
        var expired = made
            .Where(request => !string.Equals(request.Authorization, fresh, StringComparison.Ordinal))
            .ToArray();
        // One 401 per call that reached the server before the prompt finished; never a third.
        Assert.InRange(expired.Length, 1, 2);
        // The expired attempts carry the pre-prompt session's token or none (the first 401
        // clears the store before the second call's request is sent) — never a third value.
        Assert.All(expired, request => Assert.True(
            request.Authorization is null
                || string.Equals(request.Authorization, TestHarness.BearerFor(TestHarness.SessionToken), StringComparison.Ordinal),
            $"an expired attempt carried an unexpected Authorization header: {request.Authorization ?? "(none)"}"));

        // Tabs, drafts and scroll are untouched: no reset of the shell at all.
        Assert.Equal(3, h.Shell.Tabs.Count);
        Assert.False(h.Shell.IsReadOnly);
    }

    [Fact]
    public async Task CancelledReSignInReturnsToSignInStepWithTabsPreservedReadOnly()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();
        await h.Shell.OpenTabAsync("Buying", TestContext.Current.CancellationToken);
        h.Shell.Tabs[0].IsDirty = true;

        var providerGate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Connection.ReSignInProvider = () => providerGate.Task.ContinueWith(_ => false, TaskScheduler.Default);
        h.Handler.SessionResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");

        var call = h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken);
        await TestHarness.WaitUntilAsync(() => h.Connection.IsReSignInActive, "the re-sign-in prompt to open");
        providerGate.SetResult();

        await Assert.ThrowsAsync<SessionCancelledException>(() => call);

        // The prompt machinery has thrown; the shell (whose provider this test replaced)
        // was asked through the app wiring in the next test. What THIS surface proves:
        // the tabs are preserved and the token is gone. The shell's read-only state is
        // set by the shell's own provider completion — the app-wiring test below.
        Assert.Equal(2, h.Shell.Tabs.Count);
        Assert.False(h.Connection.Tokens.HasToken);
    }

    [Fact]
    public async Task CancelledReSignInMovesTheAppToTheSignInStepThroughTheAppWiring()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();
        await h.Shell.OpenTabAsync("Buying", TestContext.Current.CancellationToken);

        h.Connection.ReSignInProvider = () => h.Shell.OpenReSignInAsync();
        h.Handler.SessionResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");
        var call = h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken);
        await TestHarness.WaitUntilAsync(() => h.Shell.IsReSignInOpen, "the shell's modal to open");
        Assert.False(h.Shell.IsReadOnly, "the modal is still open, not yet read-only");

        h.Shell.CancelReSignInCommand.Execute(null);
        await Assert.ThrowsAsync<SessionCancelledException>(() => call);

        // The app stage machine moved to the sign-in step; the tabs stay in memory, read-only.
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
        Assert.Equal(2, h.Shell.Tabs.Count);
        Assert.True(h.Shell.IsReadOnly);
        Assert.False(h.Shell.IsReSignInOpen);
    }

    [Fact]
    public async Task SigningBackInFromReadOnlyRestoresWorkAndClearsTheState()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();
        await h.Shell.OpenTabAsync("Buying", TestContext.Current.CancellationToken);
        h.Connection.ReSignInProvider = () => h.Shell.OpenReSignInAsync();
        h.Handler.SessionResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");
        var call = h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken);
        await TestHarness.WaitUntilAsync(() => h.Shell.IsReSignInOpen, "the modal to open");
        h.Shell.CancelReSignInCommand.Execute(null);
        await Assert.ThrowsAsync<SessionCancelledException>(() => call);
        Assert.Equal(AppStage.Login, h.App.CurrentStage);
        Assert.True(h.Shell.IsReadOnly);

        h.Handler.SessionResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """
        {"data":{"expires_at":"2026-09-30T08:00:00Z","user":{"id":"0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f","email":"ayesha@acme.test","full_name":"Ayesha Khan"},"site":"acme.test"}}
        """);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        Assert.Equal(AppStage.Shell, h.App.CurrentStage);
        Assert.False(h.Shell.IsReadOnly);
        Assert.Equal(2, h.Shell.Tabs.Count);
        Assert.Contains(h.Shell.Tabs, tab => tab.Workspace == "Buying");
    }

    [Fact]
    public async Task AFailedRetryThrowsTheApiErrorWithoutAnotherPrompt()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        // The prompt succeeds, but the very next request expires again: the retry fails
        // with 401 auth.session_expired. No second prompt, no SessionCancelledException:
        // the caller gets its ApiRequestException to display, and its tabs stay.
        var providerCalls = 0;
        h.Connection.ReSignInProvider = () =>
        {
            providerCalls++;
            h.Handler.SessionResponder = _ => FakeServerHandler.Error(
                HttpStatusCode.Forbidden, "permission.denied", "You may not do this.");
            h.Connection.Tokens.SetToken(TestHarness.SessionToken);
            return Task.FromResult(true);
        };
        h.Handler.SessionResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");
        // The start-up sign-in already made its own session GETs: count as a delta.
        var baseline = h.Handler.RequestsFor("/api/v1/auth/session").Count();

        var failure = await Assert.ThrowsAsync<ApiRequestException>(
            () => h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken));
        Assert.Equal(1, providerCalls);
        // The one 401'd call plus its exactly-once retry, on top of the baseline.
        Assert.Equal(2, h.Handler.RequestsFor("/api/v1/auth/session").Count() - baseline);
        Assert.False(h.Connection.IsReSignInActive);
        // This test opens no tabs: the shell keeps its one home tab (no shell reset).
        Assert.Single(h.Shell.Tabs);
        Assert.Equal("permission.denied", failure.Error.Code);
    }

    [Fact]
    public async Task NonSession401CodesDoNotPrompt()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        h.Connection.ReSignInProvider = () => throw new InvalidOperationException("must not be asked");
        h.Handler.SessionResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized, "auth.totp_required", "The user has two-factor authentication.");

        await Assert.ThrowsAsync<ApiRequestException>(
            () => h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken));
        Assert.False(h.Connection.IsReSignInActive);
    }

    [Fact]
    public async Task ASecondExpiryAfterANewSignInRaisesItsOwnEvent()
    {
        using var h = new TestHarness();
        await h.CheckServerAsync();
        h.Picker.NextCommand.Execute(null);
        await h.SignInAsync();
        await h.ShellInitializedAsync();

        var signIns = 0;
        h.Connection.ReSignInProvider = () =>
        {
            signIns++;
            h.Connection.Tokens.SetToken(TestHarness.SessionToken);
            if (signIns == 1)
            {
                // The first re-sign-in starts a healthy session: the retry succeeds.
                h.Handler.SessionExpiredCode = null;
                h.Handler.SessionResponder = _ => FakeServerHandler.Json(HttpStatusCode.OK, """
                {"data":{"expires_at":"2026-09-30T08:00:00Z","user":{"id":"0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f","email":"ayesha@acme.test","full_name":"Ayesha Khan"},"site":"acme.test"}}
                """);
            }

            // The second re-sign-in's very next request expires again: the expired_code
            // stays armed, the retry 401s, and the caller gets the retry's error — while
            // the new batch still raised its own event.
            return Task.FromResult(true);
        };
        var events = 0;
        h.Connection.SessionExpired += (_, _) => events++;

        h.Handler.SessionExpiredCode = "auth.session_expired";
        await h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken);
        Assert.Equal(1, events);

        h.Handler.SessionExpiredCode = "auth.session_expired";
        h.Handler.SessionResponder = _ => FakeServerHandler.Error(
            HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");
        var eventsBefore = events;
        // A retried call that expires again throws the retry's ApiRequestException (the
        // machine's one-retry rule); its expiry still raises the new batch's own event.
        await Assert.ThrowsAsync<ApiRequestException>(
            () => h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), TestContext.Current.CancellationToken));
        Assert.Equal(eventsBefore + 1, events);
    }
}