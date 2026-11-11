using System.Text.Json;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists;

/// <summary>One row of a query page: the id, the version and the requested field values.</summary>
/// <param name="Id">The document id.</param>
/// <param name="Version">The document version (a delete needs it).</param>
/// <param name="Values">Field name to its JSON value (cloned, independent of the parse).</param>
public sealed record ListRecord(string Id, int Version, IReadOnlyDictionary<string, JsonElement> Values);

/// <summary>One page of a query.</summary>
/// <param name="Records">The rows in order.</param>
/// <param name="NextCursor">The opaque cursor of the next page; null on the last page.</param>
/// <param name="Total">The exact total, only when it was asked for.</param>
public sealed record ListPage(IReadOnlyList<ListRecord> Records, string? NextCursor, int? Total);

/// <summary>The list's seam to the server. Every method throws
/// <see cref="Spotledger.Client.Api.ApiRequestException"/> for a contract error and decides no
/// permission; the production implementation is <see cref="NativeListGateway"/>.</summary>
public interface IListGateway
{
    /// <summary>The DocType's metadata model.</summary>
    Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken);

    /// <summary>The permission summary (fields hidden or masked, document rights).</summary>
    Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken);

    /// <summary>Runs one page of the query operation. <paramref name="requestJson"/> is the
    /// contract's <c>QueryRequest</c> body (built by <see cref="ListQuery"/>).</summary>
    Task<ListPage> QueryAsync(string doctype, string requestJson, CancellationToken cancellationToken);

    /// <summary>Resolves display labels of <paramref name="ids"/> of a link target in one
    /// batch. An id that cannot be resolved is absent from the result.</summary>
    Task<IReadOnlyDictionary<string, string>> ResolveLabelsAsync(string targetDoctype, IReadOnlyCollection<string> ids, CancellationToken cancellationToken);

    /// <summary>Deletes a document at <paramref name="version"/>.</summary>
    Task DeleteAsync(string doctype, string id, int version, CancellationToken cancellationToken);

    /// <summary>Whether the bulk export operation is reachable from this gateway.</summary>
    bool SupportsBulkExport { get; }

    /// <summary>Exports the query as CSV through the bulk export endpoint (all chunks, bounded by
    /// <paramref name="maxRows"/>). Only called when <see cref="SupportsBulkExport"/>.</summary>
    Task<string> ExportCsvAsync(string doctype, IReadOnlyList<string> columns, ListFilter? filter, IReadOnlyList<ListOrder> order, int maxRows, CancellationToken cancellationToken);
}

/// <summary>Reads and writes the person's list preferences (columns, saved filters).</summary>
public interface IFilterStore
{
    /// <summary>Loads the preferences; an unreadable or absent store gives defaults.</summary>
    Task<ListPreferences> LoadAsync(string user, string doctype, CancellationToken cancellationToken);

    /// <summary>Saves the preferences.</summary>
    Task SaveAsync(string user, string doctype, ListPreferences preferences, CancellationToken cancellationToken);
}
