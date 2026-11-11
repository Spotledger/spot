namespace Spotledger.Client.Api;

/// <summary>
/// Picks the OS secret store once per app run: Windows Credential Manager, Linux
/// <c>secret-tool</c>, or the in-memory fallback when neither exists. The fallback is
/// reported exactly once through <see cref="FallbackNotice"/> — the shell shows the user
/// "You will be asked to sign in again next time".
/// </summary>
public static class SecretStores
{
    /// <summary>The warning text the shell shows when tokens cannot persist this run.</summary>
    public const string FallbackMessage = "You will be asked to sign in again next time.";

    /// <summary>Set once by <see cref="CreateDefault"/> when it had to fall back; empty otherwise.</summary>
    public static string FallbackNotice { get; private set; } = string.Empty;

    /// <summary>
    /// Chooses the store for this host. Tests inject their own store; production goes
    /// through this once at startup. A <c>secret-tool</c> missing from PATH (headless
    /// desktops without a keyring daemon) falls back with the once-only notice.
    /// </summary>
    public static ISecretStore CreateDefault()
    {
        if (OperatingSystem.IsWindows())
        {
            return new WindowsCredentialSecretStore();
        }

        if (OperatingSystem.IsLinux() && LinuxSecretToolSecretStore.IsAvailable())
        {
            return new LinuxSecretToolSecretStore();
        }

        FallbackNotice = FallbackMessage;
        return new InMemorySecretStore();
    }

    /// <summary>Clears the notice (test isolation between factory calls).</summary>
    internal static void ResetFallbackNotice() => FallbackNotice = string.Empty;
}