using System.Security.Cryptography;
using System.Text;

namespace Spotledger.Client.Workspace;

/// <summary>
/// Whose drafts these are: one signed-in user on one server. A draft is never visible to another
/// user or to another server, so every store call carries the scope and the scope is part of the
/// encryption context of every file.
/// </summary>
/// <param name="ServerId">The server the user is signed in to (its authority, as the shell names it).</param>
/// <param name="UserId">The signed-in user.</param>
public sealed record DraftScope(string ServerId, string UserId)
{
    /// <summary>Validates the parts; an empty server or user would merge scopes.</summary>
    public DraftScope Validated()
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(ServerId);
        ArgumentException.ThrowIfNullOrWhiteSpace(UserId);
        return this;
    }

    /// <summary>The name of the secret that holds this scope's draft key in the OS credential store.</summary>
    public string SecretName => "spot-drafts/" + Validated().ServerId.Trim().TrimEnd('/') + "/" + UserId.Trim();

    /// <summary>A directory-safe name for the scope: a hash, never the supplied text.</summary>
    public string DirectoryName => DraftHash.Hex("scope", Validated().ServerId.Trim().TrimEnd('/'), UserId.Trim());
}

/// <summary>
/// The identity of one stored draft: <c>(server, user, doctype, document id or new-draft id)</c>.
/// A saved document has a <see cref="DocumentId"/>; a document that has no id yet has a
/// <see cref="NewDraftId"/> that the client generated. Exactly one is set.
/// </summary>
public sealed record DraftKey
{
    /// <summary>Creates a key; throws unless exactly one of the two ids is given.</summary>
    public DraftKey(DraftScope scope, string docType, string? documentId, string? newDraftId)
    {
        ArgumentNullException.ThrowIfNull(scope);
        ArgumentException.ThrowIfNullOrWhiteSpace(docType);
        if ((documentId is null) == (newDraftId is null))
        {
            throw new ArgumentException("A draft key names a document id or a new-draft id, exactly one of them.");
        }

        Scope = scope.Validated();
        DocType = docType;
        DocumentId = documentId;
        NewDraftId = newDraftId;
    }

    /// <summary>Whose draft it is.</summary>
    public DraftScope Scope { get; }

    /// <summary>The DocType's storage name.</summary>
    public string DocType { get; }

    /// <summary>The document's id, for a stored document.</summary>
    public string? DocumentId { get; }

    /// <summary>The client's own id for a document that has none yet.</summary>
    public string? NewDraftId { get; }

    /// <summary>True for a document that has not been saved yet.</summary>
    public bool IsNewDocument => DocumentId is null;

    /// <summary>The key of a stored document's draft.</summary>
    public static DraftKey ForDocument(DraftScope scope, string docType, string documentId)
        => new(scope, docType, documentId ?? throw new ArgumentNullException(nameof(documentId)), null);

    /// <summary>The key of a new document's draft with a fresh client-generated id.</summary>
    public static DraftKey ForNew(DraftScope scope, string docType)
        => new(scope, docType, null, Guid.NewGuid().ToString("N"));

    /// <summary>The file name of the draft: a hash of the whole key, so no supplied id ever reaches a path.</summary>
    public string FileName => DraftHash.Hex("draft", Scope.ServerId.Trim().TrimEnd('/'), Scope.UserId.Trim(), DocType, DocumentId ?? string.Empty, NewDraftId ?? string.Empty) + ".draft";

    /// <summary>The same key with another id once a new document has been saved and named.</summary>
    public DraftKey WithDocumentId(string documentId) => new(Scope, DocType, documentId, null);
}

/// <summary>One line of the "Recover drafts" list.</summary>
/// <param name="Key">The draft's identity.</param>
/// <param name="Title">The document's title, when the form knew one.</param>
/// <param name="SavedAtUtc">When the draft was last written.</param>
/// <param name="ChangedFields">How many fields and tables the draft changes.</param>
public sealed record DraftSummary(DraftKey Key, string? Title, DateTimeOffset SavedAtUtc, int ChangedFields);

internal static class DraftHash
{
    /// <summary>A length-prefixed SHA-256 of the parts, in lower-case hex: the same parts always give the same name.</summary>
    public static string Hex(params string[] parts)
    {
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        foreach (var part in parts)
        {
            var bytes = Encoding.UTF8.GetBytes(part);
            hash.AppendData(BitConverter.GetBytes(bytes.Length));
            hash.AppendData(bytes);
        }

        return Convert.ToHexStringLower(hash.GetHashAndReset());
    }
}
