using System.Text.Json.Nodes;
using Spotledger.Client.Api;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms.Tests;

/// <summary>An in-memory server for one DocType: documents with versions, the contract's
/// concurrency rules, lifecycle states, and switches that make it fail the way a real one does.</summary>
public sealed class FakeGateway : IDocumentGateway
{
    public DocTypeModel Model { get; set; } = Fixtures.Model();
    public DocumentRights Rights { get; set; } = DocumentRights.All;
    public PermissionSnapshot Snapshot { get; set; } = Fixtures.Permissions();
    public Dictionary<string, JsonObject> Documents { get; } = new(StringComparer.Ordinal);
    public List<string> Calls { get; } = [];
    public List<string> Bodies { get; } = [];
    public ApiError? RejectNextWrite { get; set; }
    public bool ReferencedOnDelete { get; set; }
    public Dictionary<string, string> Derived { get; } = new(StringComparer.Ordinal);
    private int _next = 100;

    public FakeGateway Seed(string id, string state = "draft", string title = "First invoice", int version = 1)
    {
        Documents[id] = new JsonObject
        {
            ["id"] = id,
            ["version"] = version,
            ["_state"] = state,
            ["_created_by"] = "ama",
            ["_created_at"] = "2026-10-01T09:00:00Z",
            ["_modified_by"] = "ben",
            ["_modified_at"] = "2026-10-02T10:30:00Z",
            ["title"] = title,
            ["amount"] = "12.50",
            ["items"] = new JsonArray
            {
                new JsonObject { ["id"] = "r1", ["item"] = "Pen", ["qty"] = "2.00" },
                new JsonObject { ["id"] = "r2", ["item"] = "Ink", ["qty"] = "1.00" },
            },
        };
        return this;
    }

    /// <summary>Another person saves a change: the stored version moves on.</summary>
    public void OtherPersonChanges(string id, string title)
    {
        Documents[id]["title"] = title;
        Documents[id]["version"] = (int)Documents[id]["version"]! + 1;
    }

    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken)
    {
        Calls.Add("meta");
        return Task.FromResult(Model);
    }

    public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
    {
        Calls.Add("permissions");
        return Task.FromResult(new PermissionReply(Snapshot, Rights));
    }

    public Task<DocumentReply> LoadAsync(string doctype, string id, CancellationToken cancellationToken)
    {
        Calls.Add("load " + id);
        return Documents.TryGetValue(id, out var document)
            ? Task.FromResult(Reply(document))
            : throw Error("document.not_found", "There is no such document.", 404);
    }

    public Task<DocumentReply> CreateAsync(string doctype, string bodyJson, CancellationToken cancellationToken)
    {
        Calls.Add("create");
        Bodies.Add(bodyJson);
        Reject();
        var body = JsonNode.Parse(bodyJson)!.AsObject();
        var id = "NEW-" + _next++;
        var document = new JsonObject { ["id"] = id, ["version"] = 1, ["_state"] = "draft" };
        foreach (var (key, value) in body)
        {
            document[key] = value?.DeepClone();
        }

        foreach (var (key, value) in Derived)
        {
            document[key] = value;
        }

        Documents[id] = document;
        return Task.FromResult(Reply(document));
    }

    public Task<DocumentReply> PatchAsync(string doctype, string id, string patchJson, int baseVersion, CancellationToken cancellationToken)
    {
        Calls.Add($"patch {id} @{baseVersion}");
        Bodies.Add(patchJson);
        Reject();
        var document = Documents[id];
        if ((int)document["version"]! != baseVersion)
        {
            throw Error("document.stale_version", "The document changed.", 412);
        }

        foreach (var (key, value) in JsonNode.Parse(patchJson)!.AsObject())
        {
            document[key] = value?.DeepClone();
        }

        foreach (var (key, value) in Derived)
        {
            document[key] = value;
        }

        document["version"] = baseVersion + 1;
        return Task.FromResult(Reply(document));
    }

    public Task DeleteAsync(string doctype, string id, int baseVersion, CancellationToken cancellationToken)
    {
        Calls.Add($"delete {id} @{baseVersion}");
        if (ReferencedOnDelete)
        {
            throw new ApiRequestException(new ApiError
            {
                StatusCode = 409,
                Code = "links.referenced",
                Message = "Other documents refer to this one.",
                FieldErrors = [new ApiFieldError("Payment PAY-7", "links.referenced", "Pays this invoice")],
            });
        }

        Documents.Remove(id);
        return Task.CompletedTask;
    }

    public Task<DocumentReply> LifecycleAsync(string doctype, string id, LifecycleAction action, int baseVersion, string? reason, CancellationToken cancellationToken)
    {
        Calls.Add($"{action} {id} @{baseVersion}" + (reason is null ? string.Empty : $" ({reason})"));
        var document = Documents[id];
        if ((int)document["version"]! != baseVersion)
        {
            throw Error("document.stale_version", "The document changed.", 412);
        }

        document["version"] = baseVersion + 1;
        switch (action)
        {
            case LifecycleAction.Submit:
                document["_state"] = "posted";
                break;
            case LifecycleAction.Cancel:
                document["_state"] = "cancelled";
                break;
            default:
                document["_state"] = "cancelled";
                var amended = new JsonObject
                {
                    ["id"] = "AMENDED-" + _next++,
                    ["version"] = 1,
                    ["_state"] = "draft",
                    ["title"] = document["title"]?.DeepClone(),
                };
                Documents[amended["id"]!.GetValue<string>()] = amended;
                return Task.FromResult(Reply(amended));
        }

        return Task.FromResult(Reply(document));
    }

    public Task<DocumentReply> RenameAsync(string doctype, string id, string newName, int baseVersion, CancellationToken cancellationToken)
    {
        Calls.Add($"rename {id} -> {newName}");
        var document = Documents[id];
        document["version"] = baseVersion + 1;
        return Task.FromResult(Reply(document));
    }

    public ILookupSource? LookupFor(string doctype, string field, Func<LookupSnapshot> snapshot)
    {
        Calls.Add($"lookup {doctype}.{field}");
        _ = snapshot();
        return new FixedLookup();
    }

    private void Reject()
    {
        if (RejectNextWrite is { } error)
        {
            RejectNextWrite = null;
            throw new ApiRequestException(error);
        }
    }

    private static ApiRequestException Error(string code, string message, int status)
        => new(new ApiError { StatusCode = status, Code = code, Message = message });

    private static DocumentReply Reply(JsonObject document)
        => new(document.ToJsonString(), []);
}

public sealed class FixedLookup : ILookupSource
{
    public Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken)
        => Task.FromResult<IReadOnlyList<LookupItem>>([new LookupItem("CUST-1", "Customer one")]);

    public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken)
        => Task.FromResult<LookupItem?>(new LookupItem(id, id));
}

/// <summary>A shell that records what the form asked and answers from a script.</summary>
public sealed class FakeShell : IFormShell
{
    public List<string> Asked { get; } = [];
    public List<(string DocType, string? Id)> Opened { get; } = [];
    public List<Referrer> Referrers { get; } = [];
    public IReadOnlyList<string> Discarded { get; private set; } = [];
    public bool Confirm { get; set; } = true;
    public string? Text { get; set; } = "because";
    public bool Discard { get; set; }
    public ConflictChoice Choice { get; set; } = ConflictChoice.Keep;
    public int Closed { get; private set; }

    public Task<bool> ConfirmAsync(string title, string message, string confirmLabel)
    {
        Asked.Add("confirm: " + title);
        return Task.FromResult(Confirm);
    }

    public Task<string?> AskTextAsync(string title, string prompt, bool required)
    {
        Asked.Add("ask: " + title);
        return Task.FromResult(Text);
    }

    public Task<bool> ConfirmDiscardAsync(IReadOnlyList<string> changed)
    {
        Asked.Add("discard");
        Discarded = changed;
        return Task.FromResult(Discard);
    }

    public Task<ConflictChoice> ChooseConflictAsync(ConflictInfo info)
    {
        Asked.Add("conflict");
        return Task.FromResult(Choice);
    }

    public Task ShowReferrersAsync(string title, IReadOnlyList<Referrer> referrers)
    {
        Asked.Add("referrers");
        Referrers.AddRange(referrers);
        return Task.CompletedTask;
    }

    public void OpenDocument(string doctype, string? id) => Opened.Add((doctype, id));

    public void CloseTab() => Closed++;
}
