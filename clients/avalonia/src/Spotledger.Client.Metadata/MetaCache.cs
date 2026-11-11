using Microsoft.Kiota.Abstractions;
using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated.Api.V1.Meta.Item;
using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Metadata;

/// <summary>
/// The in-memory cache of the DocTypes' metadata (the brief's <c>MetaCache</c>):
/// <see cref="GetAsync"/> fetches through the <see cref="ApiConnection"/>'s generated
/// request builder, sends <c>If-None-Match</c> with the held ETag, keeps the entry when
/// the server answers 304, replaces it on a changed version, and raises
/// <see cref="MetaChanged"/> so open forms can rebuild without a restart (ADR-05
/// liveness). Memory only; nothing persists.
/// </summary>
/// <remarks>
/// The design is deliberate about the two seams this must not cross (WP-UI-07.T1's
/// owned paths exclude the Api project): the request goes straight through the public
/// generated builder <c>connection.Client.Api.V1.Meta[doctype]</c> instead of a
/// hand-written Api helper, and error mapping uses the public
/// <see cref="ApiError.FromEnvelope"/> + <see cref="ApiRequestException(ApiError)"/>
/// pair the same way <c>ApiConnection.RaiseApiError</c> does internally. The
/// metadata <em>version</em> is read from the response <em>body</em>
/// (<see cref="DocTypeMeta.Version"/>), never from the ETag header: the body is what
/// the server asserts for a 200, and a 304 carries no body at all.
/// </remarks>
public sealed class MetaCache
{
    /// <summary>Creates the cache over a connection.</summary>
    /// <param name="connection">The API connection the fetches go through.</param>
    public MetaCache(ApiConnection connection)
    {
        Connection = connection ?? throw new ArgumentNullException(nameof(connection));
    }

    /// <summary>The connection the metadata is fetched through.</summary>
    public ApiConnection Connection { get; }

    /// <summary>The cached models by doctype name.</summary>
    private readonly Dictionary<string, DocTypeModel> _cache = new(StringComparer.Ordinal);

    /// <summary>The cached ETags by doctype name (the If-None-Match the next fetch sends).</summary>
    private readonly Dictionary<string, string> _etags = new(StringComparer.Ordinal);

    /// <summary>Serialises the fetches per doctype; concurrent callers of the same
    /// doctype share one fetch.</summary>
    private readonly Dictionary<string, SemaphoreSlim> _locks = new(StringComparer.Ordinal);

    /// <summary>
    /// Raised when a doctype's metadata changed on the server: the argument is the
    /// doctype name. Open forms subscribe and rebuild with the new model.
    /// </summary>
    public event EventHandler<string>? MetaChanged;

    /// <summary>The cached model of a doctype, without fetching; null when not held.</summary>
    public DocTypeModel? Peek(string doctype) => _cache.TryGetValue(doctype, out var model) ? model : null;

    /// <summary>The ETag currently held for a doctype (the If-None-Match the next fetch
    /// sends); null when none. Starts as null: the first fetch is unconditional.</summary>
    public string? PeekETag(string doctype) => _etags.TryGetValue(doctype, out var etag) ? etag : null;

    /// <summary>
    /// Answers the doctype's model, fresh against the server: the first call fetches,
    /// later calls revalidate with <c>If-None-Match</c>; a 304 keeps the cached model
    /// (and its object identity, so forms holding it stay consistent) and never raises
    /// <see cref="MetaChanged"/>; a 200 replaces the entry, raising the event only when
    /// the metadata version actually changed. A contract error (unknown doctype,
    /// permission denied) becomes an <see cref="ApiRequestException"/>; a transport
    /// failure is retried twice before it surfaces.
    /// </summary>
    public async Task<DocTypeModel> GetAsync(string doctype, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        var gate = LockFor(doctype);
        await gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            var fresh = await FetchWithRetryAsync(doctype, PeekETag(doctype), cancellationToken).ConfigureAwait(false);
            if (fresh is null)
            {
                // 304 Not Modified: the held model is still current. (A body-less 200 —
                // the contract forbids one, but a degenerate server can answer so — with
                // no held model is a protocol failure, not a cache hit.)
                return _cache.TryGetValue(doctype, out var notModified)
                    ? notModified
                    : throw new InvalidOperationException($"The metadata fetch for {doctype} answered with no body.");
            }

            var held = _cache.TryGetValue(doctype, out var existing) ? existing : null;
            _cache[doctype] = fresh;
            if (held is null || held.Version != fresh.Version)
            {
                MetaChanged?.Invoke(this, doctype);
            }

            return fresh;
        }
        finally
        {
            gate.Release();
        }
    }

    /// <summary>Drops one doctype's entry (a renderer's explicit invalidate).</summary>
    public void Invalidate(string doctype)
    {
        _cache.Remove(doctype);
        _etags.Remove(doctype);
    }

    /// <summary>Drops every entry (a sign-out, a site switch).</summary>
    public void Clear()
    {
        _cache.Clear();
        _etags.Clear();
    }

    /// <summary>
    /// One conditional fetch. Answers the fresh model on a 200 (the ETag is remembered
    /// either way, so the next fetch can revalidate), or null on a 304. Guards the
    /// un-connected connection the way <c>ApiConnection</c> itself does, and never
    /// revalidates against an ETag the previous answer never produced a version for.
    /// </summary>
    private async Task<DocTypeModel?> FetchOnceAsync(string doctype, string? heldETag, CancellationToken cancellationToken)
    {
        if (Connection.BaseUri is null)
        {
            throw new InvalidOperationException("Call Connect(baseUri) before using the API.");
        }

        var response = await Connection.Client.Api.V1.Meta[doctype]
            .GetAsWithDoctypeGetResponseAsync(
                configuration =>
                {
                    configuration.Options.Add(new ResponseHandlerOption { ResponseHandler = new MetadataResponseHandler() });
                    if (heldETag is not null)
                    {
                        configuration.Headers.Add("If-None-Match", heldETag);
                    }
                },
                cancellationToken)
            .ConfigureAwait(false);

        if (response is null || response.Data is null)
        {
            // A 304 answers with no body, which Kiota maps to a null model. Nothing else
            // here can produce a null for a 2XX the contract defines.
            return null;
        }

        // The version comes from the BODY (the server asserts it for a 200); the quoted
        // form is kept only as the If-None-Match the next fetch sends. A body without a
        // version number produces no tag: the next fetch is unconditional again.
        var eTag = QuoteVersion(response.Data.Version);
        if (eTag is not null)
        {
            _etags[doctype] = eTag;
        }

        return MetaModelFactory.Create(response.Data, eTag);
    }

    /// <summary>The fetch call, retried on a transport failure (the network drops a
    /// packet now and then; a contract error envelope throws immediately).</summary>
    private async Task<DocTypeModel?> FetchWithRetryAsync(
        string doctype,
        string? heldETag,
        CancellationToken cancellationToken)
    {
        for (var attempt = 1; ; attempt++)
        {
            try
            {
                return await FetchOnceAsync(doctype, heldETag, cancellationToken).ConfigureAwait(false);
            }
            catch (ApiRequestException)
            {
                throw;
            }
            catch (ErrorEnvelope envelope)
            {
                // The generated builder surfaces the contract's error body (4XX/5XX) as
                // this exception. Map it with the Api project's public pair — the same
                // conversion ApiConnection.RaiseApiError performs privately — so the
                // callers see one error type.
                var error = ApiError.FromEnvelope(envelope, envelope.ResponseStatusCode);
                throw new ApiRequestException(error);
            }
            catch (Exception exception) when (attempt < MetaCacheRetryPolicy.MaxAttempts
                && exception is not OperationCanceledException and not ApiRequestException)
            {
                _ = exception;
                await Task.Delay(MetaCacheRetryPolicy.DelayFor(attempt), cancellationToken).ConfigureAwait(false);
            }
        }
    }

    /// <summary>The contract's ETag form of a metadata version: <c>"7"</c>, the quoted
    /// number (the contract's <c>ETag</c> header is <c>"&lt;version&gt;"</c>).</summary>
    private static string? QuoteVersion(int? version) => version is { } v ? $"\"{v}\"" : null;

    private SemaphoreSlim LockFor(string doctype)
    {
        lock (_locks)
        {
            if (!_locks.TryGetValue(doctype, out var gate))
            {
                gate = new SemaphoreSlim(1, 1);
                _locks[doctype] = gate;
            }

            return gate;
        }
    }
}

/// <summary>The retry policy's shape, kept explicit for the tests: two retries after an
/// initial attempt, delays 150 ms then 300 ms.</summary>
public static class MetaCacheRetryPolicy
{
    /// <summary>The attempts one <see cref="MetaCache.GetAsync"/> may spend on
    /// transport failures.</summary>
    public const int MaxAttempts = 3;

    /// <summary>The delay before the attempt with this number (1-based).</summary>
    public static TimeSpan DelayFor(int attempt) => TimeSpan.FromMilliseconds(150 * attempt);
}