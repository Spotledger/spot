using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated.Models;
using Microsoft.Kiota.Abstractions.Serialization;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>The native field lookup, using generated requests and a fresh document snapshot.
/// The host supplies saved-id resolution: a search miss is NOT proof that a document is stale.</summary>
public sealed class NativeLookupSource(
    ApiConnection connection,
    string doctype,
    string field,
    Func<LookupRequest> snapshot,
    Func<string, CancellationToken, Task<LookupItem?>> resolve) : ILookupSource
{
    /// <summary>Fetch the first suggestion page. Seam resolution remains entirely server-side.</summary>
    public async Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken)
    {
        var result = await SearchResultAsync(text, cancellationToken);
        return result.Rows.Select(row => new LookupItem(row.Id, row.Label, row.Description, row.Cells)).ToArray();
    }
    /// <summary>Decode the published response through Metadata; controls consume no generated cells.</summary>
    public async Task<LookupResult> SearchResultAsync(string text, CancellationToken cancellationToken)
    {
        var request = snapshot();
        request.Text = text;
        request.Cursor = null;
        var page = await connection.RunSessionAwareAsync(() => connection.Client.Api.V1.Lookup[doctype][field].PostAsync(request, cancellationToken: cancellationToken), cancellationToken);
        return page?.Data is null ? new([], [], false, null)
            : LookupResult.Decode(await KiotaJsonSerializer.SerializeAsStringAsync(page, false, cancellationToken));
    }
    /// <summary>Resolve through the host's authorized document reader, never by guessing from search.</summary>
    public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken) => resolve(id, cancellationToken);
}
