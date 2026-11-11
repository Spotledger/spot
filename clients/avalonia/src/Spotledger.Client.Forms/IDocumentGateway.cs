using System.Text.Json;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms;

/// <summary>A server message shown in the form's message area.</summary>
/// <param name="Level"><c>info</c>, <c>warning</c> or <c>error</c>.</param>
/// <param name="Code">The dotted code, when the server sent one.</param>
/// <param name="Text">The text to show.</param>
public sealed record FormMessage(string Level, string? Code, string Text);

/// <summary>One document as the server answered: the <c>data</c> body and the messages beside it.</summary>
/// <param name="DataJson">The document JSON (<c>id</c>, <c>version</c>, fields, child tables).</param>
/// <param name="Messages">Informational and warning messages.</param>
public sealed record DocumentReply(string DataJson, IReadOnlyList<FormMessage> Messages);

/// <summary>The document-independent rights of the signed-in user on one DocType, from
/// <c>GET /auth/permissions</c>. Hints for what to show; the server decides every operation.</summary>
public sealed record DocumentRights(
    bool Read, bool Write, bool Create, bool Delete, bool Submit, bool Cancel, bool Correct, bool Rename)
{
    /// <summary>No right at all (an unrighted DocType is omitted from the summary).</summary>
    public static DocumentRights None { get; } = new(false, false, false, false, false, false, false, false);

    /// <summary>Every right (a host that has no summary, and tests).</summary>
    public static DocumentRights All { get; } = new(true, true, true, true, true, true, true, true);

    /// <summary>Decodes <paramref name="doctype"/>'s rights from a complete summary response.</summary>
    public static DocumentRights Decode(string summaryJson, string doctype)
    {
        using var document = JsonDocument.Parse(summaryJson);
        if (!document.RootElement.TryGetProperty("data", out var data)
            || !data.TryGetProperty("doctypes", out var list))
        {
            return None;
        }

        foreach (var item in list.EnumerateArray())
        {
            if (item.TryGetProperty("doctype", out var name) && name.GetString() == doctype
                && item.TryGetProperty("rights", out var rights))
            {
                bool Right(string key) => rights.TryGetProperty(key, out var value) && value.ValueKind == JsonValueKind.True;
                return new DocumentRights(Right("read"), Right("write"), Right("create"), Right("delete"),
                    Right("submit"), Right("cancel"), Right("correct"), Right("rename"));
            }
        }

        return None;
    }
}

/// <summary>What the permission summary answered for one DocType.</summary>
/// <param name="Snapshot">The renderer-neutral snapshot the draft applies (hidden, read-only and masked fields).</param>
/// <param name="Rights">The document rights behind the toolbar.</param>
public sealed record PermissionReply(PermissionSnapshot Snapshot, DocumentRights Rights);

/// <summary>A lifecycle action on a saved document.</summary>
public enum LifecycleAction
{
    /// <summary>Post the draft (<c>submit</c> in the user's words).</summary>
    Submit,

    /// <summary>Cancel a posted document.</summary>
    Cancel,

    /// <summary>Cancel and amend a posted document in one step; the answer is the amended draft.</summary>
    Correct,
}

/// <summary>
/// The form's seam to the server. Every method throws <see cref="Spotledger.Client.Api.ApiRequestException"/>
/// for a contract error (the form maps it to fields and messages) and never decides a permission.
/// The production implementation is <see cref="NativeDocumentGateway"/>; tests supply a fake.
/// </summary>
public interface IDocumentGateway
{
    /// <summary>The DocType's metadata model.</summary>
    Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken);

    /// <summary>The permission summary for the DocType.</summary>
    Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken);

    /// <summary>Reads one document.</summary>
    Task<DocumentReply> LoadAsync(string doctype, string id, CancellationToken cancellationToken);

    /// <summary>Creates a document from a full body.</summary>
    Task<DocumentReply> CreateAsync(string doctype, string bodyJson, CancellationToken cancellationToken);

    /// <summary>Applies a merge patch against <paramref name="baseVersion"/> (<c>If-Match</c>).</summary>
    Task<DocumentReply> PatchAsync(string doctype, string id, string patchJson, int baseVersion, CancellationToken cancellationToken);

    /// <summary>Deletes a document at <paramref name="baseVersion"/>.</summary>
    Task DeleteAsync(string doctype, string id, int baseVersion, CancellationToken cancellationToken);

    /// <summary>Runs a lifecycle action at <paramref name="baseVersion"/>; <paramref name="reason"/> is sent for a cancel when given.</summary>
    Task<DocumentReply> LifecycleAsync(string doctype, string id, LifecycleAction action, int baseVersion, string? reason, CancellationToken cancellationToken);

    /// <summary>Renames a document (changes its name), at <paramref name="baseVersion"/>.</summary>
    Task<DocumentReply> RenameAsync(string doctype, string id, string newName, int baseVersion, CancellationToken cancellationToken);

    /// <summary>The lookup source of a link field, built from the draft's current snapshot; null when none is configured.</summary>
    Controls.ILookupSource? LookupFor(string doctype, string field, Func<LookupSnapshot> snapshot);
}
