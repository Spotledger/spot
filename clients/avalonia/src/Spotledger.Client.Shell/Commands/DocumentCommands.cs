namespace Spotledger.Client.Shell;

/// <summary>
/// The launcher commands of the metadata-driven document form: <c>New &lt;DocType&gt;</c> and
/// <c>Open &lt;DocType&gt;</c>. They are registered per DocType at run time (the shell learns the
/// DocTypes from the server), with ids of the form <c>document.new.&lt;doctype&gt;</c>, and they
/// resolve to a <see cref="WorkspaceTabRequest"/> for the <c>document</c> tab kind.
/// </summary>
public static class DocumentCommands
{
    /// <summary>The id prefix of the <c>New</c> commands.</summary>
    public const string NewPrefix = "document.new.";

    /// <summary>The id prefix of the <c>Open</c> commands.</summary>
    public const string OpenPrefix = "document.open.";

    /// <summary>The id of the <c>New</c> command of a DocType.</summary>
    public static string NewId(string doctype) => NewPrefix + doctype;

    /// <summary>The id of the <c>Open</c> command of a DocType.</summary>
    public static string OpenId(string doctype) => OpenPrefix + doctype;

    /// <summary>The launcher definition of <c>New &lt;label&gt;</c>.</summary>
    public static CommandDefinition New(string doctype, string label)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        return new CommandDefinition(NewId(doctype), "New " + label, $"Create a new {label} document.", "Create", string.Empty, ' ')
        {
            SearchTerms = ["new", "create", label, doctype],
        };
    }

    /// <summary>The launcher definition of <c>Open &lt;label&gt;</c> (the launcher asks for the id).</summary>
    public static CommandDefinition Open(string doctype, string label)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        return new CommandDefinition(OpenId(doctype), "Open " + label, $"Open a {label} document by its id.", "Open", string.Empty, ' ')
        {
            SearchTerms = ["open", label, doctype],
        };
    }

    /// <summary>Reads a document command id back into what it asks for.</summary>
    /// <param name="commandId">The command id.</param>
    /// <param name="isNew">True for a <c>New</c> command, false for an <c>Open</c> one.</param>
    /// <param name="doctype">The DocType the command names.</param>
    /// <returns>Whether the id is a document command.</returns>
    public static bool TryParse(string commandId, out bool isNew, out string doctype)
    {
        ArgumentNullException.ThrowIfNull(commandId);
        isNew = commandId.StartsWith(NewPrefix, StringComparison.Ordinal);
        doctype = string.Empty;
        if (isNew)
        {
            doctype = commandId[NewPrefix.Length..];
        }
        else if (commandId.StartsWith(OpenPrefix, StringComparison.Ordinal))
        {
            doctype = commandId[OpenPrefix.Length..];
        }

        return doctype.Length > 0;
    }

    /// <summary>The tab request a document command resolves to; <paramref name="id"/> is the
    /// document an <c>Open</c> command was given (ignored for <c>New</c>).</summary>
    public static WorkspaceTabRequest? RequestFor(string commandId, string? id = null)
    {
        if (!TryParse(commandId, out var isNew, out var doctype))
        {
            return null;
        }

        return isNew
            ? WorkspaceTabKinds.DocumentRequest(doctype, null)
            : id is null ? null : WorkspaceTabKinds.DocumentRequest(doctype, id);
    }
}
