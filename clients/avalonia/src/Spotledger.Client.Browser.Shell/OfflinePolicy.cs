namespace Spotledger.Client.Browser.Shell;

/// <summary>How a request is served.</summary>
public enum ServeStrategy
{
    /// <summary>Shell file or page navigation: the cache first (the key holds the content hash), the network when absent.</summary>
    CacheFirst,

    /// <summary>Everything else, data above all: only the network. The cache is neither read nor written.</summary>
    NetworkOnly,
}

/// <summary>Where the answer comes from.</summary>
public enum ServeSource
{
    /// <summary>The shell cache.</summary>
    Cache,

    /// <summary>The network.</summary>
    Network,

    /// <summary>Nothing can be served: the request fails (offline). Data is refused here, never answered stale.</summary>
    Refuse,
}

/// <summary>The policy's decision for one request.</summary>
/// <param name="Strategy">How to serve it.</param>
/// <param name="CachePath">The manifest path to read from the cache for <see cref="ServeStrategy.CacheFirst"/>; null otherwise.</param>
public sealed record ServeDecision(ServeStrategy Strategy, string? CachePath);

/// <summary>
/// The offline shell policy. The shell (the page and the files of the active build) is served from the cache
/// when the network fails; every other request, and every request that is not a plain GET, goes to the network
/// only and is never answered from, nor stored in, this cache. The service worker implements these two functions
/// and nothing more.
/// </summary>
public static class OfflinePolicy
{
    /// <summary>The page that answers every navigation.</summary>
    public const string IndexPath = "index.html";

    /// <summary>Decides how to serve a request.</summary>
    /// <param name="method">The HTTP method.</param>
    /// <param name="relativePath">The request path relative to the app root, without a leading '/'.</param>
    /// <param name="hasQuery">True when the URL has a query string (a shell file is never requested with one).</param>
    /// <param name="isNavigation">True for a page navigation.</param>
    /// <param name="isShellPath">True when the path is in the active build's cache record.</param>
    public static ServeDecision Decide(string method, string relativePath, bool hasQuery, bool isNavigation, Func<string, bool> isShellPath)
    {
        if (!string.Equals(method, "GET", StringComparison.OrdinalIgnoreCase))
        {
            return new ServeDecision(ServeStrategy.NetworkOnly, null);
        }

        if (isNavigation)
        {
            return new ServeDecision(ServeStrategy.CacheFirst, IndexPath);
        }

        if (!hasQuery && ShellManifest.IsSafePath(relativePath) && isShellPath(relativePath))
        {
            return new ServeDecision(ServeStrategy.CacheFirst, relativePath);
        }

        return new ServeDecision(ServeStrategy.NetworkOnly, null);
    }

    /// <summary>Resolves a decision against what is available.</summary>
    /// <param name="decision">The decision.</param>
    /// <param name="cacheHasEntry">True when the cache holds the entry for <see cref="ServeDecision.CachePath"/>.</param>
    /// <param name="networkAvailable">True when the network answered (or is believed up).</param>
    public static ServeSource Resolve(ServeDecision decision, bool cacheHasEntry, bool networkAvailable)
    {
        if (decision.Strategy == ServeStrategy.CacheFirst && cacheHasEntry)
        {
            return ServeSource.Cache;
        }

        return networkAvailable ? ServeSource.Network : ServeSource.Refuse;
    }
}
