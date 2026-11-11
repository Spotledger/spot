using System.Text;
using System.Text.Json;
using Microsoft.Kiota.Abstractions;
using Microsoft.Kiota.Abstractions.Serialization;
using Microsoft.Kiota.Serialization.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated.Models;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms;

/// <summary>
/// The production <see cref="IDocumentGateway"/>: the generated resource requests, run through the
/// connection's session handling, with the contract's concurrency headers (<c>If-Match</c> on every
/// change, a fresh <c>Idempotency-Key</c> on every state-changing POST). Documents cross this seam
/// as JSON text, decoded by <c>Spotledger.Client.Metadata</c>, so no generated type reaches the form.
/// </summary>
public sealed class NativeDocumentGateway(ApiConnection connection, MetaCache meta) : IDocumentGateway
{
    /// <inheritdoc />
    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken)
        => meta.GetAsync(doctype, cancellationToken);

    /// <inheritdoc />
    public async Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
    {
        var summary = await RunAsync(() => connection.Client.Api.V1.Auth.Permissions.GetAsync(
            configuration => configuration.QueryParameters.Doctype = [doctype],
            cancellationToken), cancellationToken).ConfigureAwait(false);
        var json = summary is null
            ? "{}"
            : await KiotaJsonSerializer.SerializeAsStringAsync(summary, false, cancellationToken).ConfigureAwait(false);
        return new PermissionReply(PermissionSnapshot.Decode(json), DocumentRights.Decode(json, doctype));
    }

    /// <inheritdoc />
    public async Task<DocumentReply> LoadAsync(string doctype, string id, CancellationToken cancellationToken)
        => await ReplyAsync(await RunAsync(() => connection.Client.Api.V1.Resource[doctype][id]
            .GetAsync(cancellationToken: cancellationToken), cancellationToken).ConfigureAwait(false), cancellationToken).ConfigureAwait(false);

    /// <inheritdoc />
    public async Task<DocumentReply> CreateAsync(string doctype, string bodyJson, CancellationToken cancellationToken)
    {
        var body = await ParseAsync<DocumentWrite>(bodyJson, cancellationToken).ConfigureAwait(false);
        return await ReplyAsync(await RunAsync(() => connection.Client.Api.V1.Resource[doctype].PostAsync(
            body, configuration => configuration.Headers.Add("Idempotency-Key", Guid.NewGuid().ToString("N")),
            cancellationToken), cancellationToken).ConfigureAwait(false), cancellationToken).ConfigureAwait(false);
    }

    /// <inheritdoc />
    public async Task<DocumentReply> PatchAsync(string doctype, string id, string patchJson, int baseVersion, CancellationToken cancellationToken)
    {
        var body = await ParseAsync<DocumentWrite>(patchJson, cancellationToken).ConfigureAwait(false);
        return await ReplyAsync(await RunAsync(() => connection.Client.Api.V1.Resource[doctype][id].PatchAsync(
            body, configuration => configuration.Headers.Add("If-Match", Quote(baseVersion)),
            cancellationToken), cancellationToken).ConfigureAwait(false), cancellationToken).ConfigureAwait(false);
    }

    /// <inheritdoc />
    public Task DeleteAsync(string doctype, string id, int baseVersion, CancellationToken cancellationToken)
        => RunAsync(async () =>
        {
            await connection.Client.Api.V1.Resource[doctype][id].DeleteAsync(
                configuration => configuration.Headers.Add("If-Match", Quote(baseVersion)),
                cancellationToken).ConfigureAwait(false);
            return true;
        }, cancellationToken);

    /// <inheritdoc />
    public async Task<DocumentReply> LifecycleAsync(string doctype, string id, LifecycleAction action, int baseVersion, string? reason, CancellationToken cancellationToken)
    {
        // The contract (0.3.0) has no body on cancel: the reason stays a client-side prompt
        // until the server takes one, so it is not sent.
        _ = reason;
        var item = connection.Client.Api.V1.Resource[doctype][id];
        void Headers(RequestConfiguration<DefaultQueryParameters> configuration)
        {
            configuration.Headers.Add("If-Match", Quote(baseVersion));
            configuration.Headers.Add("Idempotency-Key", Guid.NewGuid().ToString("N"));
        }

        var response = await RunAsync(() => action switch
        {
            LifecycleAction.Submit => item.PostPath.PostAsync(Headers, cancellationToken),
            LifecycleAction.Cancel => item.Cancel.PostAsync(Headers, cancellationToken),
            _ => item.Correct.PostAsync(Headers, cancellationToken),
        }, cancellationToken).ConfigureAwait(false);
        return await ReplyAsync(response, cancellationToken).ConfigureAwait(false);
    }

    /// <inheritdoc />
    public async Task<DocumentReply> RenameAsync(string doctype, string id, string newName, int baseVersion, CancellationToken cancellationToken)
    {
        var body = new Spotledger.Client.Api.Generated.Api.V1.Resource.Item.Item.Rename.RenamePostRequestBody { Name = newName };
        var response = await RunAsync(() => connection.Client.Api.V1.Resource[doctype][id].Rename.PostAsync(
            body,
            configuration =>
            {
                configuration.Headers.Add("If-Match", Quote(baseVersion));
                configuration.Headers.Add("Idempotency-Key", Guid.NewGuid().ToString("N"));
            },
            cancellationToken), cancellationToken).ConfigureAwait(false);
        return await ReplyAsync(response, cancellationToken).ConfigureAwait(false);
    }

    /// <inheritdoc />
    public ILookupSource? LookupFor(string doctype, string field, Func<LookupSnapshot> snapshot)
        => new NativeLookupSource(connection, doctype, field,
            () => SnapshotRequest(snapshot()),
            async (id, token) =>
            {
                // Saved-id resolution goes through the authorized document reader.
                var target = meta.Peek(doctype)?.Field(field)?.LinkTo;
                if (target is null)
                {
                    return null;
                }

                try
                {
                    await LoadAsync(target, id, token).ConfigureAwait(false);
                    return new LookupItem(id, id);
                }
                catch (ApiRequestException)
                {
                    return null;
                }
            });

    private static LookupRequest SnapshotRequest(LookupSnapshot snapshot)
    {
        var request = new LookupRequest { Doc = new LookupRequest_doc { AdditionalData = Scalars(snapshot.Doc) } };
        if (snapshot.Parent is { } parent)
        {
            request.Parent = new LookupRequest_parent { AdditionalData = Scalars(parent) };
        }

        return request;
    }

    /// <summary>The snapshot's scalar values as the generated request carries them (a lookup
    /// snapshot holds no child tables, so nothing nests).</summary>
    private static Dictionary<string, object> Scalars(string json)
    {
        using var document = JsonDocument.Parse(json);
        var values = new Dictionary<string, object>(StringComparer.Ordinal);
        foreach (var property in document.RootElement.EnumerateObject())
        {
            object? value = property.Value.ValueKind switch
            {
                JsonValueKind.String => property.Value.GetString(),
                JsonValueKind.True => true,
                JsonValueKind.False => false,
                JsonValueKind.Number when property.Value.TryGetInt64(out var whole) => whole,
                JsonValueKind.Number => property.Value.GetDecimal(),
                _ => null,
            };
            values[property.Name] = value!;
        }

        return values;
    }

    private static string Quote(int version) => $"\"{version}\"";

    private static async Task<T> ParseAsync<T>(string json, CancellationToken cancellationToken)
        where T : class, IParsable
        => await KiotaJsonSerializer.DeserializeAsync<T>(json, cancellationToken).ConfigureAwait(false)
           ?? throw new InvalidOperationException("The request body could not be encoded.");

    private async Task<T> RunAsync<T>(Func<Task<T>> call, CancellationToken cancellationToken)
    {
        try
        {
            return await connection.RunSessionAwareAsync(call, cancellationToken).ConfigureAwait(false);
        }
        catch (ErrorEnvelope envelope)
        {
            throw new ApiRequestException(ApiError.FromEnvelope(envelope, envelope.ResponseStatusCode));
        }
    }

    private static async Task<DocumentReply> ReplyAsync(DocumentResponse? response, CancellationToken cancellationToken)
    {
        if (response?.Data is null)
        {
            throw new ApiRequestException(ApiError.TransportFailure(0, "The server answered without a document."));
        }

        var json = await KiotaJsonSerializer.SerializeAsStringAsync(response, false, cancellationToken).ConfigureAwait(false);
        using var parsed = JsonDocument.Parse(json);
        var data = parsed.RootElement.GetProperty("data").GetRawText();
        var messages = new List<FormMessage>();
        if (parsed.RootElement.TryGetProperty("messages", out var list) && list.ValueKind == JsonValueKind.Array)
        {
            foreach (var item in list.EnumerateArray())
            {
                string Text(string key) => item.TryGetProperty(key, out var value) && value.ValueKind == JsonValueKind.String
                    ? value.GetString() ?? string.Empty
                    : string.Empty;
                messages.Add(new FormMessage(Text("level") is { Length: > 0 } level ? level : "info",
                    Text("code") is { Length: > 0 } code ? code : null, Text("message")));
            }
        }

        return new DocumentReply(data, messages);
    }
}
