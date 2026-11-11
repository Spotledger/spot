using System.Diagnostics;
using System.Runtime.Versioning;

namespace Spotledger.Client.Api;

/// <summary>
/// The GNOME/KDE keyring (Secret Service) behind "Remember me": the <c>secret-tool</c>
/// CLI from libsecret, run through a hardened <see cref="Process"/> — the executable name
/// and every argument go through <see cref="ProcessStartInfo.ArgumentList"/>, never a
/// single shell string, no shell at all. (The <c>Tmds.DBus</c> package is not referenced
/// by this solution, so the brief's fallback route applies.) Only the session token is
/// stored, keyed by its <c>spotledger-key</c> attribute. Never called on a non-Linux host.
/// </summary>
[SupportedOSPlatform("linux")]
public sealed class LinuxSecretToolSecretStore : ISecretStore
{
    /// <summary>The one collection attribute every Spotledger credential carries, so lookups address exactly one key.</summary>
    internal const string AttributeName = "spotledger-key";

    private readonly string _toolPath;

    /// <summary>Constructs the store for an executable already confirmed present (see <see cref="IsAvailable"/>).</summary>
    public LinuxSecretToolSecretStore(string toolPath = "secret-tool")
    {
        _toolPath = string.IsNullOrWhiteSpace(toolPath) ? "secret-tool" : toolPath;
    }

    /// <summary>True when <c>secret-tool</c> can be executed without a shell (it must sit on the PATH).</summary>
    public static bool IsAvailable()
    {
        var path = Environment.GetEnvironmentVariable("PATH");
        if (string.IsNullOrEmpty(path))
        {
            return false;
        }

        foreach (var directory in path.Split(Path.PathSeparator, StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            if (File.Exists(Path.Combine(directory, "secret-tool")))
            {
                return true;
            }
        }

        return false;
    }

    public async Task<string?> GetSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        var (output, error, exitCode) = await RunAsync(BuildStartInfo(_toolPath, "lookup", key), writeSecret: null, cancellationToken).ConfigureAwait(false);
        return exitCode switch
        {
            0 when output.Length == 0 => null,
            0 => output,
            _ => throw new InvalidOperationException($"The keyring refused to read the session token: {Trim(error)}"),
        };
    }

    public Task SetSecretAsync(string key, string value, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        ArgumentException.ThrowIfNullOrWhiteSpace(value);
        return SetSecretAsyncCore(key, value, cancellationToken);
    }

    private async Task SetSecretAsyncCore(string key, string value, CancellationToken cancellationToken)
    {
        var info = BuildStartInfo(_toolPath, "store", key);
        info.RedirectStandardInput = true;
        var (_, error, exitCode) = await RunAsync(info, writeSecret: value, cancellationToken).ConfigureAwait(false);
        if (exitCode != 0)
        {
            throw new InvalidOperationException($"The keyring refused to store the session token: {Trim(error)}");
        }
    }

    public async Task DeleteSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        // secret-tool exits 1 when nothing is stored under the key: deleting an absent
        // key is not an error. A daemon failure reports the same code but distinguishes
        // itself through its stderr text.
        var (_, error, exitCode) = await RunAsync(BuildStartInfo(_toolPath, "clear", key), writeSecret: null, cancellationToken).ConfigureAwait(false);
        if (exitCode != 0 && !string.IsNullOrWhiteSpace(error) && error.Contains("The name ", StringComparison.Ordinal) is false)
        {
            throw new InvalidOperationException($"The keyring refused to clear the session token: {Trim(error)}");
        }
    }

    /// <summary>Builds the hardened start info: argv-style arguments only, no shell interpretation.</summary>
    internal static ProcessStartInfo BuildStartInfo(string toolPath, string command, string key)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(toolPath);
        ArgumentException.ThrowIfNullOrWhiteSpace(command);
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        var info = new ProcessStartInfo
        {
            FileName = toolPath,
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        // Every argument is one argv element; the key can hold '/', '@' or spaces and the
        // command line is still assembled element-by-element, never as one string.
        info.ArgumentList.Add(command);
        if (command == "store")
        {
            info.ArgumentList.Add("--label=" + AttributeName + " " + key);
        }

        info.ArgumentList.Add(AttributeName);
        info.ArgumentList.Add(key);
        return info;
    }

    /// <summary>
    /// Runs <c>secret-tool</c> and returns (stdout, stderr, exit code). The optional
    /// <paramref name="writeSecret"/> goes to stdin and is never echoed back or logged.
    /// </summary>
    private static async Task<(string Output, string Error, int ExitCode)> RunAsync(
        ProcessStartInfo info, string? writeSecret, CancellationToken cancellationToken)
    {
        using var process = Process.Start(info) ?? throw new InvalidOperationException("secret-tool could not be started.");
        try
        {
            var stdout = process.StandardOutput.ReadToEndAsync(cancellationToken);
            var stderr = process.StandardError.ReadToEndAsync(cancellationToken);
            if (writeSecret is { } secret)
            {
                await process.StandardInput.WriteAsync(secret.AsMemory(), cancellationToken).ConfigureAwait(false);
                await process.StandardInput.FlushAsync(cancellationToken).ConfigureAwait(false);
                process.StandardInput.Close();
            }
            await process.WaitForExitAsync(cancellationToken).ConfigureAwait(false);
            var output = Trim(await stdout.ConfigureAwait(false));
            var errors = Trim(await stderr.ConfigureAwait(false));
            return (output, errors, process.ExitCode);
        }
        finally
        {
            if (!process.HasExited)
            {
                try
                {
                    process.Kill(entireProcessTree: true);
                }
                catch (InvalidOperationException)
                {
                    // Raced with the process exiting; nothing left to do.
                }
            }
        }
    }

    private static string Trim(string value) => value.Trim();
}