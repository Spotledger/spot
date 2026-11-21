namespace Spotledger.Client.Metadata;

/// <summary>
/// The document's own server state, decoded from the loaded document's <c>_state</c> text.
/// It says what the document <b>is</b> — a draft, a posted document, a cancelled one — and
/// nothing about what the caller may do: whether the rows are editable is the host's
/// decision (the permission summary's <c>rights.write</c> is document-independent).
/// <see cref="DocTypeModel.IsPosted"/> is a different fact: it says the DocType supports
/// posting at all.
/// </summary>
public enum DocumentStatus
{
    /// <summary>The document carries no <c>_state</c>, or a value this client does not know.</summary>
    Unknown = 0,

    /// <summary>The document is a draft (<c>draft</c>).</summary>
    Draft,

    /// <summary>The document is posted (<c>posted</c>).</summary>
    Posted,

    /// <summary>The document is cancelled (<c>cancelled</c>).</summary>
    Cancelled,
}

/// <summary>Decodes the wire's <c>_state</c> text into a <see cref="DocumentStatus"/>.</summary>
internal static class DocumentStatusText
{
    /// <summary>
    /// Decodes the raw text; a value the client does not know (and absent text) answers
    /// <see cref="DocumentStatus.Unknown"/>, and the caller keeps the text verbatim.
    /// </summary>
    public static DocumentStatus Decode(string? raw)
    {
        if (string.IsNullOrWhiteSpace(raw))
        {
            return DocumentStatus.Unknown;
        }

        var text = raw.Trim();
        if (string.Equals(text, "draft", StringComparison.OrdinalIgnoreCase))
        {
            return DocumentStatus.Draft;
        }

        if (string.Equals(text, "posted", StringComparison.OrdinalIgnoreCase))
        {
            return DocumentStatus.Posted;
        }

        return string.Equals(text, "cancelled", StringComparison.OrdinalIgnoreCase)
            ? DocumentStatus.Cancelled
            : DocumentStatus.Unknown;
    }
}
