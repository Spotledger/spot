namespace Spotledger.Client.Designer;

/// <summary>
/// The ordered list of tools the designer shell offers beside the DocType designer.
/// </summary>
/// <remarks>
/// The composition root fills it (in the desktop host that composes this designer, and in the tests
/// that build one by hand); the shell only reads it, so the designer never has to know that the
/// report builder exists. Order is registration order, which is the order the shell lists and tabs
/// the tools in.
/// </remarks>
public sealed class DesignerToolRegistry
{
    private readonly List<IDesignerTool> _tools = new();

    /// <summary>Creates an empty registry.</summary>
    public DesignerToolRegistry()
    {
    }

    /// <summary>Creates a registry from tools, in the order given.</summary>
    /// <param name="tools">The tools to register.</param>
    public DesignerToolRegistry(IEnumerable<IDesignerTool> tools)
    {
        ArgumentNullException.ThrowIfNull(tools);
        foreach (var tool in tools)
        {
            Register(tool);
        }
    }

    /// <summary>Every registered tool, in registration order.</summary>
    public IReadOnlyList<IDesignerTool> Tools => _tools;

    /// <summary>Whether a tool with this id is registered.</summary>
    public bool Contains(string id) => Find(id) is not null;

    /// <summary>The tool with this id, or null.</summary>
    public IDesignerTool? Find(string id) =>
        id is null ? null : _tools.FirstOrDefault(t => string.Equals(t.Id, id, StringComparison.Ordinal));

    /// <summary>
    /// Registers a tool at the end of the list. A second tool with the same id is refused: two
    /// buttons that open different things under one id would make the shell's navigation ambiguous
    /// (and a tool opened from a link would become a coin toss).
    /// </summary>
    /// <param name="tool">The tool to add.</param>
    /// <exception cref="ArgumentException">The id is empty or already registered, or the title is empty.</exception>
    public DesignerToolRegistry Register(IDesignerTool tool)
    {
        ArgumentNullException.ThrowIfNull(tool);
        ArgumentException.ThrowIfNullOrEmpty(tool.Id);
        ArgumentException.ThrowIfNullOrEmpty(tool.Title);
        if (Contains(tool.Id))
        {
            throw new ArgumentException($"A tool with the id `{tool.Id}` is already registered; ids must be unique.", nameof(tool));
        }

        _tools.Add(tool);
        return this;
    }
}
