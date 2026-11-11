namespace Spotledger.Client.Browser;

/// <summary>
/// The key/value storage the browser head uses (the page's <c>localStorage</c> in production). It is a
/// seam so the start-up wiring is tested with a fake: nothing here knows about JavaScript. A browser
/// may refuse storage (private window, blocked site data): an implementation reports that by
/// returning <see cref="IsAvailable"/> false and reading as empty, never by throwing at the caller.
/// Only non-secret conveniences are stored (the recent server URLs); a session token never is.
/// </summary>
public interface IBrowserStorage
{
    /// <summary>True when writes are kept. False means every read is empty and writes are dropped.</summary>
    bool IsAvailable { get; }

    /// <summary>The stored text for <paramref name="key"/>, or null when there is none.</summary>
    string? Read(string key);

    /// <summary>Stores <paramref name="value"/> under <paramref name="key"/>.</summary>
    void Write(string key, string value);

    /// <summary>Removes <paramref name="key"/>; a missing key is not an error.</summary>
    void Delete(string key);
}

/// <summary>An in-process storage: the fallback when the browser refuses storage, and the test double.</summary>
public sealed class MemoryBrowserStorage : IBrowserStorage
{
    private readonly Dictionary<string, string> _values = new(StringComparer.Ordinal);

    /// <inheritdoc />
    public bool IsAvailable => true;

    /// <inheritdoc />
    public string? Read(string key) => _values.GetValueOrDefault(key);

    /// <inheritdoc />
    public void Write(string key, string value) => _values[key] = value;

    /// <inheritdoc />
    public void Delete(string key) => _values.Remove(key);
}

/// <summary>The storage of a browser that refuses it: reads are empty, writes are dropped.</summary>
public sealed class UnavailableBrowserStorage : IBrowserStorage
{
    /// <inheritdoc />
    public bool IsAvailable => false;

    /// <inheritdoc />
    public string? Read(string key) => null;

    /// <inheritdoc />
    public void Write(string key, string value)
    {
    }

    /// <inheritdoc />
    public void Delete(string key)
    {
    }
}
