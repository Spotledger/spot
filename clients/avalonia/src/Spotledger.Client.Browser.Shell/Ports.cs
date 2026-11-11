namespace Spotledger.Client.Browser.Shell;

/// <summary>
/// The persistent store of the shell cache (the Cache API in the browser, a dictionary in tests). It keeps
/// two things: blobs by key, and small named text records (the version record). Nothing here is for data
/// responses: only shell files and hash-keyed metadata or view documents are ever put in it.
/// </summary>
public interface IShellStorage
{
    /// <summary>The blob stored under <paramref name="key"/>, or null.</summary>
    Task<byte[]?> GetAsync(string key, CancellationToken cancellationToken = default);

    /// <summary>Stores a blob, replacing any earlier one.</summary>
    Task PutAsync(string key, byte[] bytes, CancellationToken cancellationToken = default);

    /// <summary>Removes a blob; a missing key is not an error.</summary>
    Task DeleteAsync(string key, CancellationToken cancellationToken = default);

    /// <summary>The keys that start with <paramref name="prefix"/>, in ordinal order.</summary>
    Task<IReadOnlyList<string>> ListKeysAsync(string prefix, CancellationToken cancellationToken = default);

    /// <summary>The text record named <paramref name="name"/>, or null.</summary>
    Task<string?> ReadRecordAsync(string name, CancellationToken cancellationToken = default);

    /// <summary>Stores a text record, replacing any earlier one.</summary>
    Task WriteRecordAsync(string name, string value, CancellationToken cancellationToken = default);

    /// <summary>Removes a text record; a missing record is not an error.</summary>
    Task DeleteRecordAsync(string name, CancellationToken cancellationToken = default);
}

/// <summary>The network as the shell sees it: one file of the published build, by manifest path.</summary>
public interface IShellNetwork
{
    /// <summary>
    /// Downloads the file at <paramref name="path"/> (relative to the app root, as listed in the manifest).
    /// Throws <see cref="ShellNetworkException"/> when the network or the server fails.
    /// </summary>
    Task<byte[]> FetchAsync(string path, CancellationToken cancellationToken = default);
}

/// <summary>Loads the assemblies of one module into the running app (WebAssembly in the browser).</summary>
public interface IModuleLoader
{
    /// <summary>
    /// Loads <paramref name="assemblies"/>, every one already checked to be listed in the shell manifest.
    /// Throws <see cref="ShellNetworkException"/> when a file cannot be fetched.
    /// </summary>
    Task LoadAsync(IReadOnlyList<ShellEntry> assemblies, CancellationToken cancellationToken = default);
}

/// <summary>The SHA-256 of a byte array as lower-case hex; a port so a host without the algorithm can supply one.</summary>
public interface IContentHasher
{
    /// <summary>The SHA-256 of <paramref name="bytes"/>, 64 lower-case hex characters.</summary>
    string Sha256Hex(ReadOnlySpan<byte> bytes);
}

/// <summary>The default hasher, over the base class library.</summary>
public sealed class Sha256ContentHasher : IContentHasher
{
    /// <inheritdoc />
    public string Sha256Hex(ReadOnlySpan<byte> bytes) =>
        Convert.ToHexStringLower(System.Security.Cryptography.SHA256.HashData(bytes));
}

/// <summary>The network or the server failed to deliver a file. The message is plain and safe to show.</summary>
public sealed class ShellNetworkException : Exception
{
    /// <summary>Creates the exception.</summary>
    public ShellNetworkException(string message) : base(message)
    {
    }

    /// <summary>Creates the exception with the cause.</summary>
    public ShellNetworkException(string message, Exception inner) : base(message, inner)
    {
    }
}

/// <summary>
/// A manifest, a downloaded file or a cached document failed an integrity check. Always fatal for the step
/// that raised it: the shell fails closed and never uses the content.
/// </summary>
public sealed class ShellIntegrityException : Exception
{
    /// <summary>Creates the exception.</summary>
    public ShellIntegrityException(string message) : base(message)
    {
    }
}
