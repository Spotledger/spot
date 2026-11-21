using Spotledger.Client.Api;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The OS secret-store plumbing (WP-UI-02.T3 deliverable 1): key shape, round-trip over
/// the in-memory fake and — when this host has a running keyring (CI's Fedora runner does,
/// headless desktops may not) — a live <c>secret-tool</c> integration; the factory's
/// fallback notice; and the token redaction surface (<see cref="Spotledger.Client.Api.SessionTokenStore"/>'s
/// ToString prints presence, never the token).
/// </summary>
public sealed class SecretStoreTests
{
    [Theory]
    [InlineData("https://site.example", "ayesha@acme.test", "https://site.example/ayesha@acme.test")]
    [InlineData("https://site.example/", "ayesha", "https://site.example/ayesha")]
    [InlineData("http://127.0.0.55:8080", "adam", "http://127.0.0.55:8080/adam")]
    public void KeysAreServerSlashUser(string server, string user, string expected)
    {
        Assert.Equal(expected, Spotledger.Client.Api.SecretKey.For(server, user));
    }

    [Fact]
    public async Task TheInMemoryStoreRoundTripsAndDeletes()
    {
        var store = new InMemorySecretStore();
        var token = TestContext.Current.CancellationToken;

        Assert.Null(await store.GetSecretAsync("s/u", token));
        await store.SetSecretAsync("s/u", "token-1", token);
        Assert.Equal("token-1", await store.GetSecretAsync("s/u", token));
        await store.SetSecretAsync("s/u", "token-2", token);
        Assert.Equal("token-2", await store.GetSecretAsync("s/u", token));
        await store.DeleteSecretAsync("s/u", token);
        Assert.Null(await store.GetSecretAsync("s/u", token));
        // Deleting an absent key is not an error.
        await store.DeleteSecretAsync("s/u", token);
        Assert.Equal([("s/u", "token-1"), ("s/u", "token-2")], store.Writes);
    }

    [Fact]
    public async Task TheLinuxStoreStoresLooksupAndClearsWhenAKeyringExists()
    {
        // The live keyring test: skipped (not failed) where the tool or a reachable
        // keyring daemon is missing. The platform guard satisfies the
        // platform-compatibility analyzer.
        if (!OperatingSystem.IsLinux() || !LinuxSecretToolSecretStore.IsAvailable())
        {
            return;
        }
        // A running CI/desktop session can carry the tool without a reachable daemon
        // (gnome-keyring/kwallet absent): a set would fail the store, not the code.
        try
        {
            var probe = new LinuxSecretToolSecretStore();
            await probe.GetSecretAsync("http://probe.invalid/spotledger-keyring-probe", TestContext.Current.CancellationToken);
        }
        catch (InvalidOperationException)
        {
            return;
        }

        var store = new LinuxSecretToolSecretStore();
        var key = "http://spotledger-tests.invalid/" + Guid.NewGuid().ToString("N");
        var token = TestContext.Current.CancellationToken;

        try
        {
            Assert.Null(await store.GetSecretAsync(key, token));
            await store.SetSecretAsync(key, "token-abc", token);
            Assert.Equal("token-abc", await store.GetSecretAsync(key, token));
            await store.DeleteSecretAsync(key, token);
            Assert.Null(await store.GetSecretAsync(key, token));
        }
        finally
        {
            await store.DeleteSecretAsync(key, token);
        }
    }

    [Fact]
    public void TheHardenedStartInfoPassesArgumentsAsAnArrayNeverAShellString()
    {
        if (!OperatingSystem.IsLinux())
        {
            return;
        }

        var info = LinuxSecretToolSecretStore.BuildStartInfo("secret-tool", "store", "http://s.test/a b/c");

        Assert.Equal("secret-tool", info.FileName);
        Assert.False(info.UseShellExecute);
        Assert.Equal(["store", "--label=spotledger-key http://s.test/a b/c", "spotledger-key", "http://s.test/a b/c"], info.ArgumentList);
    }

    [Fact]
    public void TheFactoryFallsBackToMemoryWithTheOnceNoticeWhenNoStoreExists()
    {
        SecretStores.ResetFallbackNotice();
        if (OperatingSystem.IsWindows() || (OperatingSystem.IsLinux() && LinuxSecretToolSecretStore.IsAvailable()))
        {
            // This host has a real store; the fallback branch cannot be exercised here.
            return;
        }

        var store = SecretStores.CreateDefault();
        Assert.IsType<InMemorySecretStore>(store);
        Assert.Equal(SecretStores.FallbackMessage, SecretStores.FallbackNotice);
        Assert.Equal("You will be asked to sign in again next time.", LoginViewModel.RememberMeNotice);
    }

    [Fact]
    public void TheTokenStoreNeverRendersTheTokenItself()
    {
        var store = new SessionTokenStore();
        Assert.Equal("SessionTokenStore(empty)", store.ToString());
        store.SetToken("3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs");
        Assert.Equal("SessionTokenStore(1 token held)", store.ToString());
        Assert.DoesNotContain("3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs", store.ToString(), StringComparison.Ordinal);
    }

    [Fact]
    public async Task TheBearerProviderHandsTheTokenOnlyToTheTransportLayer()
    {
        var store = new SessionTokenStore();
        store.SetToken("3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs");
        var header = await store.GetAuthorizationTokenAsync(new Uri("http://s.test"), cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal("3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs", header);
        store.ClearToken();
        Assert.Equal(string.Empty, await store.GetAuthorizationTokenAsync(new Uri("http://s.test"), cancellationToken: TestContext.Current.CancellationToken));
    }
}