using System.Security.Cryptography;
using Spotledger.Client.Api;

namespace Spotledger.Client.Workspace;

/// <summary>Where and how big. The defaults suit a workstation.</summary>
public sealed record FileDraftStoreOptions
{
    /// <summary>The folder drafts live under (one sub-folder per user and server). Required.</summary>
    public required string Root { get; init; }

    /// <summary>The largest one draft may be, in bytes of its encrypted file. Default 1 MiB.</summary>
    public long MaxDraftBytes { get; init; } = 1024 * 1024;

    /// <summary>The most one user's drafts on one server may take together. Default 32 MiB; the oldest go first.</summary>
    public long MaxScopeBytes { get; init; } = 32L * 1024 * 1024;

    /// <summary>The default root: the user's local data folder.</summary>
    public static string DefaultRoot()
        => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Spotledger", "drafts");
}

/// <summary>
/// One encrypted file per draft under the user's local data folder. The AES-GCM key is generated once
/// per user and server and kept in the OS credential store (the store of WP-UI-02.T3); it is never
/// written to a file. When no key can be kept (no OS store, or only the in-memory fallback), drafts stay
/// in memory for this run and <see cref="MemoryOnlyNotice"/> says so.
/// </summary>
/// <remarks>
/// Files are created owner-only on Unix (0600, folders 0700). On Windows the folder is under the user's
/// profile, whose ACL already limits it to that user. A write goes to a temp file, is flushed, then
/// renamed over the draft, so a crash leaves either the old file or the new one. A file that cannot be
/// opened (damaged, wrong key, unknown format, belongs to another key) is moved to a <c>quarantine</c>
/// folder with a note and never crashes the client.
/// </remarks>
public sealed class FileDraftStore : IDraftStore, IDisposable
{
    /// <summary>What the UI says when drafts are not written to disk.</summary>
    public const string MemoryOnlyMessage = "Drafts are not saved to disk on this machine.";

    private const string Extension = ".draft";
    private const string QuarantineFolder = "quarantine";

    private readonly FileDraftStoreOptions _options;
    private readonly ISecretStore _secrets;
    private readonly TimeProvider _clock;
    private readonly InMemoryDraftStore _memory = new();
    private readonly Dictionary<string, byte[]?> _keys = new(StringComparer.Ordinal);
    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly List<QuarantineNote> _quarantined = [];

    /// <summary>Creates the store.</summary>
    /// <param name="options">Where to store and the caps.</param>
    /// <param name="secrets">The OS credential store that keeps the keys.</param>
    /// <param name="clock">The clock for quarantine notes.</param>
    public FileDraftStore(FileDraftStoreOptions options, ISecretStore secrets, TimeProvider? clock = null)
    {
        ArgumentNullException.ThrowIfNull(options);
        ArgumentException.ThrowIfNullOrWhiteSpace(options.Root);
        if (options.MaxDraftBytes <= 0 || options.MaxScopeBytes < options.MaxDraftBytes)
        {
            throw new ArgumentOutOfRangeException(nameof(options), "The per-server cap must be at least the per-draft cap.");
        }

        _options = options;
        _secrets = secrets ?? throw new ArgumentNullException(nameof(secrets));
        _clock = clock ?? TimeProvider.System;
    }

    /// <summary>Releases the store's lock.</summary>
    public void Dispose() => _gate.Dispose();

    /// <inheritdoc />
    public string? MemoryOnlyNotice => _keys.Values.Any(k => k is null) ? MemoryOnlyMessage : null;

    /// <inheritdoc />
    public IReadOnlyList<QuarantineNote> Quarantined
    {
        get
        {
            lock (_quarantined)
            {
                return [.. _quarantined];
            }
        }
    }

    /// <inheritdoc />
    public event EventHandler<QuarantineNote>? DraftQuarantined;

    /// <inheritdoc />
    public async Task<bool> IsPersistentAsync(DraftScope scope, CancellationToken cancellationToken = default)
    {
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            return await KeyForAsync(scope, cancellationToken).ConfigureAwait(false) is not null;
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    /// <inheritdoc />
    public async Task<DraftSaveResult> SaveAsync(DraftRecord record, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(record);
        var scope = record.Key.Scope;
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            var key = await KeyForAsync(scope, cancellationToken).ConfigureAwait(false);
            if (key is null)
            {
                return await _memory.SaveAsync(record, cancellationToken).ConfigureAwait(false);
            }

            var bytes = DraftCrypto.Seal(key, scope, DraftRecordJson.Write(record));
            if (bytes.Length > _options.MaxDraftBytes)
            {
                return new DraftSaveResult(DraftSaveOutcome.TooLarge, "This document has too many changes to keep as a draft.");
            }

            try
            {
                var directory = EnsureDirectory(scope);
                var path = Path.GetFullPath(Path.Combine(directory, record.Key.FileName));
                WriteAtomically(directory, path, bytes);
                File.SetLastWriteTimeUtc(path, record.SavedAtUtc.UtcDateTime);
                var evicted = Evict(directory, path);
                return new DraftSaveResult(DraftSaveOutcome.Saved, evicted > 0 ? "Older drafts were removed to stay within the size limit." : null);
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                return new DraftSaveResult(DraftSaveOutcome.Failed, "The draft could not be written: " + ex.GetType().Name + ".");
            }
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    /// <inheritdoc />
    public async Task<DraftRecord?> LoadAsync(DraftKey key, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(key);
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            var aes = await KeyForAsync(key.Scope, cancellationToken).ConfigureAwait(false);
            if (aes is null)
            {
                return await _memory.LoadAsync(key, cancellationToken).ConfigureAwait(false);
            }

            var path = Path.Combine(DirectoryOf(key.Scope), key.FileName);
            return File.Exists(path) ? Read(aes, key.Scope, path, key.FileName) : null;
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    /// <inheritdoc />
    public async Task DeleteAsync(DraftKey key, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(key);
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            await _memory.DeleteAsync(key, cancellationToken).ConfigureAwait(false);
            TryDelete(Path.Combine(DirectoryOf(key.Scope), key.FileName));
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    /// <inheritdoc />
    public async Task<IReadOnlyList<DraftSummary>> ListAsync(DraftScope scope, CancellationToken cancellationToken = default)
    {
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            var aes = await KeyForAsync(scope, cancellationToken).ConfigureAwait(false);
            if (aes is null)
            {
                return await _memory.ListAsync(scope, cancellationToken).ConfigureAwait(false);
            }

            var directory = DirectoryOf(scope);
            var summaries = new List<DraftSummary>();
            if (Directory.Exists(directory))
            {
                foreach (var path in Directory.EnumerateFiles(directory, "*" + Extension))
                {
                    cancellationToken.ThrowIfCancellationRequested();
                    var record = Read(aes, scope, path, Path.GetFileName(path));
                    if (record is not null && !record.IsEmpty)
                    {
                        summaries.Add(new DraftSummary(record.Key, record.Title, record.SavedAtUtc, record.ChangedCount));
                    }
                }
            }

            return [.. summaries.OrderByDescending(s => s.SavedAtUtc)];
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    /// <inheritdoc />
    public async Task DeleteAllAsync(DraftScope scope, CancellationToken cancellationToken = default)
    {
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            await _memory.DeleteAllAsync(scope, cancellationToken).ConfigureAwait(false);
            var directory = DirectoryOf(scope);
            if (Directory.Exists(directory))
            {
                try
                {
                    Directory.Delete(directory, recursive: true);
                }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
                {
                    // Delete what can be deleted, one file at a time: a locked file must not keep the rest.
                    foreach (var file in Directory.EnumerateFiles(directory, "*", SearchOption.AllDirectories))
                    {
                        TryDelete(file);
                    }
                }
            }

            _ = _keys.Remove(scope.DirectoryName);
            try
            {
                await _secrets.DeleteSecretAsync(scope.SecretName, cancellationToken).ConfigureAwait(false);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                // The credential store is unavailable: the files are gone, so the key opens nothing.
            }
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    // ------------------------------------------------------------------ key

    /// <summary>The scope's key, created on first use; null when drafts must stay in memory. Call under the gate.</summary>
    private async Task<byte[]?> KeyForAsync(DraftScope scope, CancellationToken cancellationToken)
    {
        var name = scope.DirectoryName;
        if (_keys.TryGetValue(name, out var cached))
        {
            return cached;
        }

        byte[]? key = null;
        try
        {
            if (_secrets is not InMemorySecretStore)
            {
                var stored = await _secrets.GetSecretAsync(scope.SecretName, cancellationToken).ConfigureAwait(false);
                if (stored is null)
                {
                    var fresh = DraftCrypto.NewKey();
                    await _secrets.SetSecretAsync(scope.SecretName, Convert.ToBase64String(fresh), cancellationToken).ConfigureAwait(false);
                    key = fresh;
                }
                else if (TryDecodeKey(stored, out var decoded))
                {
                    key = decoded;
                }
            }
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            key = null;
        }

        _keys[name] = key;
        return key;
    }

    private static bool TryDecodeKey(string text, out byte[] key)
    {
        key = [];
        try
        {
            var bytes = Convert.FromBase64String(text);
            if (bytes.Length != DraftCrypto.KeyBytes)
            {
                return false;
            }

            key = bytes;
            return true;
        }
        catch (FormatException)
        {
            return false;
        }
    }

    // ------------------------------------------------------------------ files

    private string DirectoryOf(DraftScope scope) => Path.Combine(_options.Root, scope.DirectoryName);

    private string EnsureDirectory(DraftScope scope)
    {
        var directory = DirectoryOf(scope);
        if (OperatingSystem.IsWindows())
        {
            _ = Directory.CreateDirectory(_options.Root);
            _ = Directory.CreateDirectory(directory);
        }
        else
        {
            const UnixFileMode ownerOnly = UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute;
            _ = Directory.CreateDirectory(_options.Root, ownerOnly);
            _ = Directory.CreateDirectory(directory, ownerOnly);
        }

        return directory;
    }

    private static void WriteAtomically(string directory, string path, byte[] bytes)
    {
        var temp = Path.Combine(directory, Guid.NewGuid().ToString("N") + ".tmp");
        try
        {
            var options = new FileStreamOptions
            {
                Mode = FileMode.CreateNew,
                Access = FileAccess.Write,
                Share = FileShare.None,
            };
            if (!OperatingSystem.IsWindows())
            {
                options.UnixCreateMode = UnixFileMode.UserRead | UnixFileMode.UserWrite;
            }

            using (var stream = new FileStream(temp, options))
            {
                stream.Write(bytes);
                stream.Flush(flushToDisk: true);
            }

            File.Move(temp, path, overwrite: true);
        }
        catch
        {
            TryDelete(temp);
            throw;
        }
    }

    private DraftRecord? Read(byte[] aesKey, DraftScope scope, string path, string fileName)
    {
        byte[] file;
        try
        {
            file = File.ReadAllBytes(path);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // Cannot be read right now (locked): leave it where it is.
            return null;
        }

        try
        {
            var record = DraftRecordJson.Read(DraftCrypto.Open(aesKey, scope, file));
            if (!record.Key.Scope.Equals(scope) || !string.Equals(record.Key.FileName, fileName, StringComparison.Ordinal))
            {
                throw new InvalidDataException("The draft belongs to another document.");
            }

            return record;
        }
        catch (CryptographicException)
        {
            Quarantine(scope, path, fileName, "The draft could not be decrypted (damaged, or made with another key).");
        }
        catch (InvalidDataException ex)
        {
            Quarantine(scope, path, fileName, ex.Message);
        }

        return null;
    }

    private void Quarantine(DraftScope scope, string path, string fileName, string reason)
    {
        var now = _clock.GetUtcNow();
        try
        {
            var folder = Path.Combine(Path.GetDirectoryName(path)!, QuarantineFolder);
            if (OperatingSystem.IsWindows())
            {
                _ = Directory.CreateDirectory(folder);
            }
            else
            {
                _ = Directory.CreateDirectory(folder, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
            }

            var target = Path.Combine(folder, fileName + "." + now.ToUnixTimeMilliseconds() + ".bad");
            File.Move(path, target, overwrite: true);
            File.WriteAllText(target + ".note", reason + Environment.NewLine);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            TryDelete(path);
        }

        var note = new QuarantineNote(scope, fileName, reason, now);
        lock (_quarantined)
        {
            _quarantined.Add(note);
        }

        DraftQuarantined?.Invoke(this, note);
    }

    /// <summary>Deletes the oldest drafts of the folder until the total is within the cap (never the one just written); returns how many went.</summary>
    private int Evict(string directory, string keep)
    {
        var info = new DirectoryInfo(directory);
        foreach (var stale in info.EnumerateFiles("*.tmp").Where(f => f.LastWriteTimeUtc < _clock.GetUtcNow().UtcDateTime.AddHours(-1)))
        {
            TryDelete(stale.FullName);
        }

        var files = info.EnumerateFiles("*" + Extension).ToList();
        var total = files.Sum(f => f.Length);
        var evicted = 0;
        if (total <= _options.MaxScopeBytes)
        {
            return 0;
        }

        foreach (var file in files
                     .Where(f => !string.Equals(f.FullName, keep, StringComparison.Ordinal))
                     .OrderBy(f => f.LastWriteTimeUtc)
                     .ThenBy(f => f.Name, StringComparer.Ordinal))
        {
            if (total <= _options.MaxScopeBytes)
            {
                break;
            }

            total -= file.Length;
            TryDelete(file.FullName);
            evicted++;
        }

        return evicted;
    }

    private static void TryDelete(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // Best effort: a locked file is removed next time.
        }
    }
}
