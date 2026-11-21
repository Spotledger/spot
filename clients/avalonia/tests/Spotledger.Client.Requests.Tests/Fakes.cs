using System.Text.Json.Nodes;
using Spotledger.Client.Api;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Requests.Tests;

internal static class Clock
{
    public static readonly DateTimeOffset Now = new(2026, 10, 23, 9, 0, 0, TimeSpan.Zero);
}

internal static class Texts
{
    public static RequestText English(string? singular = null, string? plural = null)
    {
        var catalogue = new MemoryRunLabelCatalogue();
        if (singular is not null)
        {
            catalogue.Set(RunLabel.SingularKey, "en", "acme", singular).Set(RunLabel.PluralKey, "en", "acme", plural ?? singular + "s");
        }

        return For("en", singular is null ? null : "acme", catalogue);
    }

    public static RequestText For(string locale, string? organisation = null, IRunLabelCatalogue? catalogue = null)
        => new(locale, new RunLabelResolver(catalogue).Resolve(locale, organisation));
}

/// <summary>The server's side of the request endpoints, in memory.</summary>
internal sealed class FakeRequests : IRequestsGateway
{
    public Dictionary<string, EntryInfo> Entries { get; } = new(StringComparer.Ordinal);
    public Dictionary<RequestFilter, List<RequestSummary>> Mine { get; } = [];
    public Dictionary<string, RequestDetail> Details { get; } = new(StringComparer.Ordinal);
    public StepInfo Step { get; set; } = new("T-1", "R-1", "Your details", "Fill in the title.", null, "note", "N-1");
    public ApiError? CompleteError { get; set; }
    public List<string> Calls { get; } = [];

    public Task<EntryInfo> GetEntryInfoAsync(string recordType, CancellationToken cancellationToken)
        => Task.FromResult(Entries[recordType]);

    public Task<StartedRequest> StartAsync(string processId, CancellationToken cancellationToken)
    {
        Calls.Add("start " + processId);
        return Task.FromResult(new StartedRequest("R-1", Step));
    }

    public Task<StepInfo> GetStepAsync(string taskId, CancellationToken cancellationToken) => Task.FromResult(Step);

    public Task CompleteStepAsync(string taskId, CancellationToken cancellationToken)
    {
        Calls.Add("complete " + taskId);
        if (CompleteError is { } error)
        {
            CompleteError = null;
            throw new ApiRequestException(error);
        }

        return Task.CompletedTask;
    }

    public bool CancelThrowsNetwork { get; set; }

    public Func<RequestFilter, Task>? ListGate { get; set; }

    public async Task<IReadOnlyList<RequestSummary>> ListMineAsync(RequestFilter filter, CancellationToken cancellationToken)
    {
        Calls.Add("list " + filter);
        if (ListGate is { } gate) { await gate(filter); }

        return Mine.TryGetValue(filter, out var rows) ? rows : [];
    }

    public Task<RequestDetail> GetRequestAsync(string requestId, CancellationToken cancellationToken) => Task.FromResult(Details[requestId]);

    public Task UndoAsync(string requestId, CancellationToken cancellationToken)
    {
        Calls.Add("undo " + requestId);
        return Task.CompletedTask;
    }

    public Task CancelAsync(string requestId, string? reason, CancellationToken cancellationToken)
    {
        Calls.Add("cancel " + requestId);
        if (CancelThrowsNetwork) { throw new System.Net.Http.HttpRequestException("https://x.example/?token=SECRET"); }

        return Task.CompletedTask;
    }
}

/// <summary>A one-DocType document server: a note with a required title.</summary>
internal sealed class FakeDocuments : IDocumentGateway
{
    public Dictionary<string, JsonObject> Documents { get; } = new(StringComparer.Ordinal)
    {
        ["N-1"] = new JsonObject { ["id"] = "N-1", ["version"] = 1, ["_state"] = "draft", ["title"] = string.Empty },
    };

    public List<string> Calls { get; } = [];

    public static DocTypeModel Model() => new("note", "Note", 1, true,
    [
        new FieldModel("title", "Title", FieldKind.Text, required: true),
    ]);

    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken) => Task.FromResult(Model());

    public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
        => Task.FromResult(new PermissionReply(
            PermissionSnapshot.Decode("""{"etag":"e1","data":{"doctypes":[{"doctype":"note","rights":{"create":true,"write":true},"fields":{}}]}}"""),
            DocumentRights.All));

    public Task<DocumentReply> LoadAsync(string doctype, string id, CancellationToken cancellationToken)
        => Task.FromResult(new DocumentReply(Documents[id].ToJsonString(), []));

    public Task<DocumentReply> CreateAsync(string doctype, string bodyJson, CancellationToken cancellationToken)
        => throw new InvalidOperationException("A form step edits the run's draft; it never creates one.");

    public Task<DocumentReply> PatchAsync(string doctype, string id, string patchJson, int baseVersion, CancellationToken cancellationToken)
    {
        Calls.Add("patch " + patchJson);
        var document = Documents[id];
        foreach (var (key, value) in JsonNode.Parse(patchJson)!.AsObject())
        {
            document[key] = value?.DeepClone();
        }

        document["version"] = baseVersion + 1;
        return Task.FromResult(new DocumentReply(document.ToJsonString(), []));
    }

    public Task DeleteAsync(string doctype, string id, int baseVersion, CancellationToken cancellationToken) => Task.CompletedTask;

    public Task<DocumentReply> LifecycleAsync(string doctype, string id, LifecycleAction action, int baseVersion, string? reason, CancellationToken cancellationToken)
        => throw new NotSupportedException();

    public Task<DocumentReply> RenameAsync(string doctype, string id, string newName, int baseVersion, CancellationToken cancellationToken)
        => throw new NotSupportedException();

    public Controls.ILookupSource? LookupFor(string doctype, string field, Func<LookupSnapshot> snapshot) => null;
}
