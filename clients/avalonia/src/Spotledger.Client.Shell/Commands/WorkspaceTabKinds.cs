namespace Spotledger.Client.Shell;

/// <summary>A request to open a workspace tab of some kind.</summary>
/// <param name="Kind">The tab kind (<c>document</c> for the metadata-driven form).</param>
/// <param name="Key">The unique key: asking for a tab whose key is already open selects it.</param>
/// <param name="Title">The tab title until the content has its own.</param>
/// <param name="Arguments">What the kind needs to build its content.</param>
public sealed record WorkspaceTabRequest(string Kind, string Key, string Title, IReadOnlyDictionary<string, string> Arguments);

/// <summary>
/// The registry of workspace tab kinds. A kind is registered once with a factory that builds the
/// tab's content from a request; the shell asks the registry to open a request, and the registry
/// answers an already open tab (same key) instead of a second one.
/// </summary>
public sealed class WorkspaceTabKinds
{
    /// <summary>The metadata-driven document form.</summary>
    public const string Document = "document";

    private readonly Dictionary<string, Func<WorkspaceTabRequest, object>> _factories = new(StringComparer.Ordinal);
    private readonly Dictionary<string, object> _open = new(StringComparer.Ordinal);
    private int _newDocuments;

    /// <summary>The unique key of a document tab; a new document gets a key of its own.</summary>
    public static string DocumentKey(string doctype, string? id)
        => id is null ? $"document:{doctype}:new" : $"document:{doctype}:{id}";

    /// <summary>The request that opens a document (<paramref name="id"/> null: a new one).</summary>
    public static WorkspaceTabRequest DocumentRequest(string doctype, string? id, string? title = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        var arguments = new Dictionary<string, string>(StringComparer.Ordinal) { ["doctype"] = doctype };
        if (id is not null)
        {
            arguments["id"] = id;
        }

        return new WorkspaceTabRequest(Document, DocumentKey(doctype, id), title ?? (id is null ? "New " + doctype : doctype + " " + id), arguments);
    }

    /// <summary>The DocType and id a document request carries.</summary>
    public static (string DocType, string? Id) ParseDocumentRequest(WorkspaceTabRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        var doctype = request.Arguments.TryGetValue("doctype", out var d) ? d : throw new ArgumentException("A document request names its DocType.", nameof(request));
        return (doctype, request.Arguments.TryGetValue("id", out var id) ? id : null);
    }

    /// <summary>Registers a kind. A second registration of the same kind is refused.</summary>
    public void Register(string kind, Func<WorkspaceTabRequest, object> factory)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(kind);
        ArgumentNullException.ThrowIfNull(factory);
        if (!_factories.TryAdd(kind, factory))
        {
            throw new InvalidOperationException($"The tab kind '{kind}' is already registered.");
        }
    }

    /// <summary>Whether a kind is registered.</summary>
    public bool IsRegistered(string kind) => _factories.ContainsKey(kind);

    /// <summary>
    /// Opens a request: the already open content for its key, else a new one from the kind's
    /// factory. A new document is never reused (each "New" opens its own tab).
    /// </summary>
    public object Open(WorkspaceTabRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        if (!_factories.TryGetValue(request.Kind, out var factory))
        {
            throw new InvalidOperationException($"No tab kind '{request.Kind}' is registered.");
        }

        var isNew = request.Kind == Document && !request.Arguments.ContainsKey("id");
        if (!isNew && _open.TryGetValue(request.Key, out var existing))
        {
            return existing;
        }

        var content = factory(request);
        var key = isNew ? $"{request.Key}#{++_newDocuments}" : request.Key;
        _open[key] = content;
        return content;
    }

    /// <summary>Forgets an open tab (it was closed). Returns whether the key was open.</summary>
    public bool Close(string key) => _open.Remove(key);

    /// <summary>The open content for a key, or null.</summary>
    public object? Find(string key) => _open.TryGetValue(key, out var content) ? content : null;
}
