using Spotledger.Client.Designer;

namespace Spotledger.Designer.Reports;

/// <summary>One builder the designer shell offers next to the DocType designer.</summary>
/// <param name="Id">The stable id (for navigation and tests).</param>
/// <param name="Title">The name in the shell's tool list.</param>
/// <param name="Description">One line saying what it builds.</param>
/// <param name="Factory">
/// Creates the tool's view-model from the shell's neutral context: the connection (adapted here to
/// this project's <see cref="IBuilderServer"/>) and the draft id, or the app name for the wizard.
/// </param>
public sealed record BuilderTool(string Id, string Title, string Description, Func<DesignerToolContext, object> Factory) : IDesignerTool
{
    /// <summary>
    /// The shell's way of creating the tool. This project, not the shell, knows that the connection
    /// has to be an <see cref="IBuilderServer"/>; <see cref="DesignerToolContext.RequireServer{T}"/>
    /// fails with a sentence when the host handed the designer something else.
    /// </summary>
    /// <param name="context">The connection and the item the tool edits.</param>
    object IDesignerTool.Create(DesignerToolContext context) => Factory(context);

    /// <summary>The context's connection as this project's builder server.</summary>
    internal static IBuilderServer Server(DesignerToolContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        return context.RequireServer<IBuilderServer>();
    }
}

/// <summary>
/// The builders this project registers into the designer shell: the shell lists
/// <see cref="All"/> beside the DocType designer and calls <see cref="IDesignerTool.Create"/> when a
/// tool is opened. The host fills the registry (see <see cref="DesignerTools.Default"/>); the shell
/// never learns that these builders exist.
/// </summary>
public static class BuilderTools
{
    /// <summary>The report builder.</summary>
    public static BuilderTool Report { get; } = new(
        "report-builder",
        "Report builder",
        "Build a report from a record type, without code.",
        context => new ReportBuilderViewModel(BuilderTool.Server(context), context.ItemId));

    /// <summary>The dashboard builder.</summary>
    public static BuilderTool Dashboard { get; } = new(
        "dashboard-builder",
        "Dashboard builder",
        "Arrange tiles from your reports on a dashboard.",
        context => new DashboardBuilderViewModel(BuilderTool.Server(context), context.ItemId));

    /// <summary>The approval wizard (the context's item is the app the workflow belongs to).</summary>
    public static BuilderTool Approval { get; } = new(
        "approval-wizard",
        "Approval workflow",
        "Ask someone to approve a document, with a time limit.",
        context => new ApprovalWizardViewModel(BuilderTool.Server(context), context.ItemId));

    /// <summary>Every builder, in the order the shell lists them.</summary>
    public static IReadOnlyList<BuilderTool> All { get; } = new[] { Report, Dashboard, Approval };
}

/// <summary>
/// The one call the host makes: build the registry the designer shell reads.
/// </summary>
/// <remarks>
/// The shell (<c>Spotledger.Client.Designer</c>) may not reference this project, so the composition
/// root registers these builders itself and hands the registry to the designer view-model. A fresh
/// registry is returned each time, so a host that registers its own tools as well cannot change
/// what another window shows.
/// </remarks>
public static class DesignerTools
{
    /// <summary>A registry holding the three builders, in the order the shell lists them.</summary>
    public static DesignerToolRegistry Default()
    {
        var registry = new DesignerToolRegistry();
        foreach (var tool in BuilderTools.All)
        {
            registry.Register(tool);
        }

        return registry;
    }
}
