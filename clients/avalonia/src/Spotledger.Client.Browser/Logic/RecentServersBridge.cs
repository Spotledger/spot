using Spotledger.Client.Shell;

namespace Spotledger.Client.Browser;

/// <summary>
/// Keeps the shell's recent-server list across page loads. <see cref="RecentServersStore"/> writes a
/// JSON file; in the browser that file lives in the runtime's in-memory file system, which a reload
/// empties. The bridge copies the stored text from the browser storage into that file before the store
/// is built (<see cref="Hydrate"/>) and copies the file back after the user picked a server
/// (<see cref="Persist"/>). Only server URLs cross: never a user name, token or password.
/// </summary>
public sealed class RecentServersBridge
{
    /// <summary>The storage key holding the recent-server JSON text.</summary>
    public const string StorageKey = "spot.browser.recentServers";

    private readonly IBrowserStorage _storage;
    private readonly string _filePath;

    /// <summary>Creates the bridge.</summary>
    /// <param name="storage">The browser storage.</param>
    /// <param name="filePath">The file the <see cref="RecentServersStore"/> reads and writes.</param>
    public RecentServersBridge(IBrowserStorage storage, string filePath)
    {
        ArgumentNullException.ThrowIfNull(storage);
        ArgumentException.ThrowIfNullOrWhiteSpace(filePath);
        _storage = storage;
        _filePath = filePath;
    }

    /// <summary>The file path the store must be built on.</summary>
    public string FilePath => _filePath;

    /// <summary>Writes the stored list into the file. With nothing stored the file is left alone.</summary>
    public void Hydrate()
    {
        var text = _storage.Read(StorageKey);
        if (string.IsNullOrWhiteSpace(text))
        {
            return;
        }

        var directory = Path.GetDirectoryName(_filePath);
        if (!string.IsNullOrEmpty(directory))
        {
            Directory.CreateDirectory(directory);
        }

        File.WriteAllText(_filePath, text);
    }

    /// <summary>Copies the file's text into the storage. A missing or unreadable file stores nothing.</summary>
    public void Persist()
    {
        try
        {
            if (File.Exists(_filePath))
            {
                _storage.Write(StorageKey, File.ReadAllText(_filePath));
            }
        }
        catch (IOException)
        {
            // The list is a convenience: a failed copy keeps the previous stored text.
        }
        catch (UnauthorizedAccessException)
        {
        }
    }
}
