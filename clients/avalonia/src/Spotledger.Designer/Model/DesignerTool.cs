namespace Spotledger.Client.Designer;

/// <summary>
/// What a registered tool is given when the shell opens it: the connection the designer already
/// holds, and the item the tool works on.
/// </summary>
/// <remarks>
/// The shell cannot name the server type a tool talks to. The report builder's server surface lives
/// in <c>Spotledger.Designer.Reports</c>, which references this project, so this project may not
/// reference it back. The shell therefore passes the connection it has as an object and the tool
/// adapts it (see <see cref="RequireServer{T}"/>); the shell never learns what the tool does with it.
/// <para>
/// <see cref="ItemId"/> is the id of the thing the tool edits: a draft id for the report and
/// dashboard builders, and the app a workflow belongs to for the approval wizard.
/// </para>
/// </remarks>
public sealed record DesignerToolContext
{
    /// <summary>Creates the context.</summary>
    /// <param name="server">The connection the designer holds; the tool adapts it to its own surface.</param>
    /// <param name="itemId">The draft (or app) the tool edits.</param>
    public DesignerToolContext(object server, string itemId)
    {
        Server = server ?? throw new ArgumentNullException(nameof(server));
        ArgumentException.ThrowIfNullOrEmpty(itemId);
        ItemId = itemId;
    }

    /// <summary>The connection the tool adapts to its own server surface.</summary>
    public object Server { get; }

    /// <summary>The draft (or app) the tool edits.</summary>
    public string ItemId { get; }

    /// <summary>
    /// The connection as the surface this tool talks to, or a plain explanation when the host
    /// handed the shell a different one. A tool that cannot adapt fails with a sentence, never a
    /// cast exception in the middle of opening a tab.
    /// </summary>
    /// <typeparam name="T">The server surface the tool needs.</typeparam>
    public T RequireServer<T>()
        where T : class =>
        Server as T
            ?? throw new InvalidOperationException(
                $"This tool needs a {typeof(T).Name} connection and the designer was given a {Server.GetType().Name}.");
}

/// <summary>
/// One tool the designer shell lists beside the DocType designer. The shell knows only this: what
/// the tool is called and how to create its view-model. Who fills the registry is the composition
/// root, not the shell.
/// </summary>
public interface IDesignerTool
{
    /// <summary>The stable id, used for navigation and by the tests.</summary>
    string Id { get; }

    /// <summary>The name in the shell's tool list.</summary>
    string Title { get; }

    /// <summary>One line saying what the tool builds.</summary>
    string Description { get; }

    /// <summary>
    /// Creates the tool's view-model over the host's connection. The shell shows it through the
    /// application's data templates (a view-model type with no template shows nothing but its
    /// description, which is why the host registers one per tool).
    /// </summary>
    /// <param name="context">The connection and the item the tool edits.</param>
    object Create(DesignerToolContext context);
}
