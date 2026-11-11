using System.Net;
using Spotledger.Client.Api;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The re-sign-in race (WP-TST-14.T7, issue #1892): a 401 answered for a request sent under
/// one token generation can reach the shared prompt gate after another call's prompt has
/// already installed a newer session. Left alone, the late caller sees no pending prompt and
/// opens a SECOND re-sign-in modal right after the user signed in — two prompts and two
/// <see cref="ApiConnection.SessionExpired"/> events for one expiry batch.
/// <para>
/// Every interleaving here is forced with gates the test releases itself (a
/// <see cref="TaskCompletionSource"/> the provider awaits, the handler's
/// <see cref="FakeServerHandler.BeforeResponseAsync"/> hold, and the product's own
/// between-the-check-and-the-gate seam <c>ApiConnection.BeforeReSignInGateAsync</c>): no
/// sleeps and no timing windows, so the tests fail for the bug and for nothing else.
/// </para>
/// </summary>
public sealed class ReSignInRaceTests
{
    private static readonly string FreshBearer = TestHarness.BearerFor(TestHarness.RefreshedSessionToken);

    /// <summary>Signs in through the picker and the login form and waits for the shell's own session load.</summary>
    private static async Task<TestHarness> SignedInAsync()
    {
        var harness = new TestHarness();
        await harness.CheckServerAsync();
        harness.Picker.NextCommand.Execute(null);
        await harness.SignInAsync();
        await harness.ShellInitializedAsync();
        return harness;
    }

    /// <summary>Answers <see cref="TestHarness.SessionBody"/> only to <paramref name="expectedBearer"/>; every other request expires.</summary>
    private static Func<HttpRequestMessage, HttpResponseMessage> SessionAnsweringOnlyWith(string? expectedBearer) =>
        request => expectedBearer is not null
            && request.Headers.TryGetValues("Authorization", out var values)
            && values.Contains(expectedBearer, StringComparer.Ordinal)
                ? FakeServerHandler.Json(HttpStatusCode.OK, TestHarness.SessionBody)
                : FakeServerHandler.Error(
                    HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");

    /// <summary>The same answer for the hand-rolled contract-0.3 endpoint (204 for the live session).</summary>
    private static Func<HttpRequestMessage, HttpResponseMessage> PasswordAnsweringOnlyWith(string? expectedBearer) =>
        request => expectedBearer is not null
            && request.Headers.TryGetValues("Authorization", out var values)
            && values.Contains(expectedBearer, StringComparer.Ordinal)
                ? new HttpResponseMessage(HttpStatusCode.NoContent)
                : FakeServerHandler.Error(
                    HttpStatusCode.Unauthorized, "auth.session_expired", "The session has ended; sign in again.");

    /// <summary>
    /// The reproduction of the reported race on the generated path. Two authorized calls are
    /// in flight under generation G and both are answered 401. The first opens the modal; the
    /// second is held at the product's own seam — the point between "this 401 is current" and
    /// the gate — until the first call's prompt has completed and generation G+1 is installed.
    /// The second call must then take its single retry, not ask the user to sign in again.
    /// </summary>
    [Fact]
    public async Task ALate401AfterTheRenewalDoesNotOpenASecondPrompt()
    {
        using var h = await SignedInAsync();
        var cancellationToken = TestContext.Current.CancellationToken;

        var providerCalls = 0;
        var expiryEvents = 0;
        var promptOpen = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var tokenInstalled = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Connection.ReSignInProvider = async () =>
        {
            Interlocked.Increment(ref providerCalls);
            // The user is at the modal: the test decides when the fresh session lands.
            await promptOpen.Task.ConfigureAwait(false);
            h.Connection.Tokens.SetToken(TestHarness.RefreshedSessionToken);
            tokenInstalled.TrySetResult();
            return true;
        };
        h.Connection.SessionExpired += (_, _) => Interlocked.Increment(ref expiryEvents);
        h.Handler.SessionResponder = SessionAnsweringOnlyWith(FreshBearer);
        var baseline = h.Handler.RequestsFor("/api/v1/auth/session").Count();

        var gateEntries = 0;
        var secondAtGate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var releaseSecond = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Connection.BeforeReSignInGateAsync = () =>
        {
            // The first caller walks straight in and opens the modal; the second is parked in
            // exactly the window the bug lives in.
            if (Interlocked.Increment(ref gateEntries) == 2)
            {
                secondAtGate.TrySetResult();
                return releaseSecond.Task;
            }

            return Task.CompletedTask;
        };

        var callA = h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), cancellationToken);
        var callB = h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), cancellationToken);

        await secondAtGate.Task.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken);
        // Both 401s are in; the modal belongs to the first of them.
        await TestHarness.WaitUntilAsync(
            () => h.Connection.IsReSignInActive && Volatile.Read(ref providerCalls) == 1,
            "the first caller's re-sign-in modal",
            timeoutMilliseconds: 30000);

        promptOpen.SetResult();
        await tokenInstalled.Task.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken);
        // The prompt's teardown cleared the pending task before the second caller is let go:
        // this is "B reaches the gate after the prompt completed".
        await TestHarness.WaitUntilAsync(
            () => !h.Connection.IsReSignInActive,
            "the shared prompt to close",
            timeoutMilliseconds: 30000);
        releaseSecond.SetResult();

        var results = await Task.WhenAll(callA, callB);
        Assert.All(results, result => Assert.Equal("Ayesha Khan", result.User!.FullName));
        // THE assertion of the bug: the late call opened a second modal (and raised a second event).
        Assert.Equal(1, providerCalls);
        Assert.Equal(1, expiryEvents);

        var made = h.Handler.RequestsFor("/api/v1/auth/session").Skip(baseline).ToArray();
        // One expired request and one retry per call — never a third attempt for the late one.
        Assert.Equal(4, made.Length);
        Assert.Equal(2, made.Count(request => string.Equals(request.Authorization, FreshBearer, StringComparison.Ordinal)));
    }

    /// <summary>
    /// The hand-rolled contract-0.3 path through <c>SendWithExpiryAsync</c> behaves the same:
    /// a request sent under generation G that is answered 401 after another call's prompt
    /// installed G+1 is stale. It must not clear the new session, must not raise a second
    /// event, must not open a second prompt, and must retry once with the new bearer.
    /// </summary>
    [Fact]
    public async Task AHandRolled401FromTheOldGenerationIsStaleAndRetriesOnce()
    {
        using var h = await SignedInAsync();
        var cancellationToken = TestContext.Current.CancellationToken;

        var providerCalls = 0;
        var expiryEvents = 0;
        var promptOpen = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var tokenInstalled = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Connection.ReSignInProvider = async () =>
        {
            Interlocked.Increment(ref providerCalls);
            await promptOpen.Task.ConfigureAwait(false);
            h.Connection.Tokens.SetToken(TestHarness.RefreshedSessionToken);
            tokenInstalled.TrySetResult();
            return true;
        };
        h.Connection.SessionExpired += (_, _) => Interlocked.Increment(ref expiryEvents);
        h.Handler.PasswordResponder = PasswordAnsweringOnlyWith(FreshBearer);

        // The second hand-rolled request is sent under the old token but answered only after
        // the prompt installed the fresh one: the server's 401 for a request of the old batch.
        var passwordArrivals = 0;
        var secondHeld = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var releaseSecond = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        h.Handler.BeforeResponseAsync = (request, _) =>
        {
            if (!string.Equals(request.RequestUri!.AbsolutePath, "/api/v1/auth/password", StringComparison.Ordinal))
            {
                return Task.CompletedTask;
            }

            if (Interlocked.Increment(ref passwordArrivals) == 2)
            {
                secondHeld.TrySetResult();
                return releaseSecond.Task;
            }

            return Task.CompletedTask;
        };

        var callA = h.Connection.ChangePasswordAsync("old-passphrase-01", "new-passphrase-02", cancellationToken);
        var callB = h.Connection.ChangePasswordAsync("old-passphrase-01", "new-passphrase-02", cancellationToken);

        await secondHeld.Task.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken);
        await TestHarness.WaitUntilAsync(
            () => h.Connection.IsReSignInActive && Volatile.Read(ref providerCalls) == 1,
            "the first caller's re-sign-in modal",
            timeoutMilliseconds: 30000);

        promptOpen.SetResult();
        await tokenInstalled.Task.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken);
        await TestHarness.WaitUntilAsync(
            () => !h.Connection.IsReSignInActive,
            "the shared prompt to close",
            timeoutMilliseconds: 30000);
        releaseSecond.SetResult();

        // Neither call throws: each gets its one retry under the renewed session.
        var failure = await Record.ExceptionAsync(() => Task.WhenAll(callA, callB));
        // The stale 401 neither prompted again nor raised a second event.
        Assert.Equal(1, providerCalls);
        Assert.Null(failure);
        Assert.Equal(1, expiryEvents);
        // The renewed session survived the stale 401 (the old code cleared it unconditionally).
        Assert.True(h.Connection.Tokens.HasToken, "the renewed session is still held");
        Assert.Equal(TestHarness.RefreshedSessionToken, h.Connection.Tokens.GetTokenForSecretStore());

        var made = h.Handler.RequestsFor("/api/v1/auth/password").ToArray();
        // Expired attempt plus one retry, per call; the retries carry the renewed bearer.
        Assert.Equal(4, made.Length);
        Assert.Equal(2, made.Count(request => string.Equals(request.Authorization, FreshBearer, StringComparison.Ordinal)));
    }

    /// <summary>
    /// The gate-release bug: the old cleanup called <c>_reSignInGate.WaitAsync</c> without
    /// awaiting it and then released the semaphore, so a contended cleanup released a permit
    /// it never held and two callers could sit inside the gate at once. The observable
    /// invariant is that every expiry batch gets exactly one prompt — including two batches
    /// that follow each other without any other work in between.
    /// </summary>
    [Fact]
    public async Task TwoSequentialExpiryBatchesEachGetExactlyOnePrompt()
    {
        using var h = await SignedInAsync();
        var cancellationToken = TestContext.Current.CancellationToken;

        var providerCalls = 0;
        var expiryEvents = 0;
        // The session the fake answers 200 to; null means "nothing is live, everything expires".
        string? liveBearer = null;
        h.Connection.ReSignInProvider = () =>
        {
            Interlocked.Increment(ref providerCalls);
            liveBearer = FreshBearer;
            h.Connection.Tokens.SetToken(TestHarness.RefreshedSessionToken);
            return Task.FromResult(true);
        };
        h.Connection.SessionExpired += (_, _) => Interlocked.Increment(ref expiryEvents);
        h.Handler.SessionResponder = request => SessionAnsweringOnlyWith(liveBearer)(request);

        // Batch one: the expired session, one prompt, one retry.
        var first = await h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), cancellationToken);
        Assert.Equal("Ayesha Khan", first.User!.FullName);
        Assert.Equal(1, providerCalls);
        Assert.Equal(1, expiryEvents);
        Assert.False(h.Connection.IsReSignInActive);

        // The renewed session expires too: a fresh batch, its own event and its own one prompt.
        liveBearer = null;
        var second = await h.Connection.RunSessionAwareAsync(() => h.Connection.LoadSessionAsync(), cancellationToken);
        Assert.Equal("Ayesha Khan", second.User!.FullName);
        Assert.Equal(2, providerCalls);
        Assert.Equal(2, expiryEvents);
        Assert.False(h.Connection.IsReSignInActive);

        // Exactly one prompt per batch: never a third for the two calls, and the modal closed
        // after each batch (no caller was left behind in the gate).
        var expired = h.Handler.RequestsFor("/api/v1/auth/session")
            .Count(request => !string.Equals(request.Authorization, FreshBearer, StringComparison.Ordinal));
        Assert.Equal(2, expired);
    }
}
