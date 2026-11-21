using System.Text.Json;
using Microsoft.Kiota.Abstractions.Serialization;
using Microsoft.Kiota.Serialization.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated.Models;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists;

/// <summary>
/// The production <see cref="IListGateway"/>: the generated <c>resource/{doctype}/query</c> request
/// through the connection's session handling. Meta, permissions and delete go through the form's
/// gateway so both screens share one transport. Bulk export is not offered here: the generated
/// client does not surface the chunk cursor header, so export falls back to the loaded rows
/// (see the report's open questions).
/// </summary>
public sealed class NativeListGateway(ApiConnection connection, IDocumentGateway documents) : IListGateway
{
    /// <inheritdoc />
    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken)
        => documents.GetMetaAsync(doctype, cancellationToken);

    /// <inheritdoc />
    public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
        => documents.GetPermissionsAsync(doctype, cancellationToken);

    /// <inheritdoc />
    public async Task<ListPage> QueryAsync(string doctype, string requestJson, CancellationToken cancellationToken)
    {
        var body = await KiotaJsonSerializer.DeserializeAsync<QueryRequest>(requestJson, cancellationToken).ConfigureAwait(false)
            ?? throw new InvalidOperationException("The query could not be encoded.");
        DocumentPage? page;
        try
        {
            page = await connection.RunSessionAwareAsync(
                () => connection.Client.Api.V1.Resource[doctype].Query.PostAsync(body, cancellationToken: cancellationToken),
                cancellationToken).ConfigureAwait(false);
        }
        catch (ErrorEnvelope envelope)
        {
            throw new ApiRequestException(ApiError.FromEnvelope(envelope, envelope.ResponseStatusCode));
        }

        if (page is null)
        {
            throw new ApiRequestException(ApiError.TransportFailure(0, "The server answered without a page."));
        }

        var json = await KiotaJsonSerializer.SerializeAsStringAsync(page, false, cancellationToken).ConfigureAwait(false);
        return ListPageReader.Read(json);
    }

    /// <inheritdoc />
    public async Task<IReadOnlyDictionary<string, string>> ResolveLabelsAsync(string targetDoctype, IReadOnlyCollection<string> ids, CancellationToken cancellationToken)
    {
        var labels = new Dictionary<string, string>(StringComparer.Ordinal);
        if (ids.Count == 0)
        {
            return labels;
        }

        try
        {
            var request = new ListQuery(["id", "name"], new InFilter("id", ids.Select(ListScalar.Of).ToArray()))
                .ToRequestJson(Math.Min(ids.Count, ListLimits.MaxPageSize), null, false);
            var page = await QueryAsync(targetDoctype, request, cancellationToken).ConfigureAwait(false);
            foreach (var record in page.Records)
            {
                if (record.Values.TryGetValue("name", out var name) && name.ValueKind == JsonValueKind.String && name.GetString() is { Length: > 0 } text)
                {
                    labels[record.Id] = text;
                }
            }
        }
        catch (ApiRequestException)
        {
            // A target the person cannot read, or one without a name: the ids stay as they are.
        }

        return labels;
    }

    /// <inheritdoc />
    public Task DeleteAsync(string doctype, string id, int version, CancellationToken cancellationToken)
        => documents.DeleteAsync(doctype, id, version, cancellationToken);

    /// <inheritdoc />
    public bool SupportsBulkExport => false;

    /// <inheritdoc />
    public Task<string> ExportCsvAsync(string doctype, IReadOnlyList<string> columns, ListFilter? filter, IReadOnlyList<ListOrder> order, int maxRows, CancellationToken cancellationToken)
        => throw new NotSupportedException("Bulk export is not available through this gateway.");
}

/// <summary>Reads a <c>DocumentPage</c> JSON body.</summary>
public static class ListPageReader
{
    /// <summary>Parses the page; rows without an id are skipped (they cannot be opened).</summary>
    public static ListPage Read(string json)
    {
        using var document = JsonDocument.Parse(json);
        var root = document.RootElement;
        var records = new List<ListRecord>();
        if (root.TryGetProperty("data", out var data) && data.ValueKind == JsonValueKind.Array)
        {
            foreach (var row in data.EnumerateArray())
            {
                if (row.ValueKind != JsonValueKind.Object || !row.TryGetProperty("id", out var id) || id.ValueKind != JsonValueKind.String)
                {
                    continue;
                }

                var version = row.TryGetProperty("version", out var v) && v.ValueKind == JsonValueKind.Number && v.TryGetInt32(out var n) ? n : 0;
                var values = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
                foreach (var property in row.EnumerateObject())
                {
                    if (property.Name is not ("id" or "version"))
                    {
                        values[property.Name] = property.Value.Clone();
                    }
                }

                records.Add(new ListRecord(id.GetString()!, version, values));
            }
        }

        var next = root.TryGetProperty("next_cursor", out var cursor) && cursor.ValueKind == JsonValueKind.String ? cursor.GetString() : null;
        int? total = root.TryGetProperty("total", out var t) && t.ValueKind == JsonValueKind.Number && t.TryGetInt32(out var count) ? count : null;
        return new ListPage(records, next, total);
    }
}
