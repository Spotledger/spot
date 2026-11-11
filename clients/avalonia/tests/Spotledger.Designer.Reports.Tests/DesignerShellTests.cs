using Spotledger.Client.Designer;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

/// <summary>
/// The designer shell's tool list, filled the way the host fills it: the three builders appear
/// beside the DocType designer, in order, and opening one creates its view-model over the
/// connection the host already holds.
/// </summary>
public sealed class DesignerShellTests
{
    private static DesignerToolListViewModel Shell(FakeBuilderServer server, DesignerToolRegistry? registry = null, string item = "dash-1") =>
        new(registry ?? DesignerTools.Default(), new DesignerToolContext(server, item));

    [Fact]
    public void The_shell_lists_the_three_builders_in_the_order_the_host_registered_them()
    {
        var shell = Shell(new FakeBuilderServer());

        Assert.True(shell.HasTools);
        Assert.Equal(
            ["Report builder", "Dashboard builder", "Approval workflow"],
            shell.Tools.Select(t => t.Title));
        Assert.Equal(
            ["report-builder", "dashboard-builder", "approval-wizard"],
            shell.Tools.Select(t => t.Id));
        Assert.All(shell.Tools, row => Assert.False(string.IsNullOrWhiteSpace(row.Description)));
        Assert.False(shell.HasActiveTool);
    }

    [Fact]
    public async Task Opening_a_builder_from_the_shell_creates_it_over_the_hosts_connection()
    {
        var server = new FakeBuilderServer();
        var shell = Shell(server, item: "dash-1");

        var dashboard = Assert.IsType<DashboardBuilderViewModel>(shell.Open("dashboard-builder"));
        Assert.True(shell.HasActiveTool);
        Assert.True(shell.Tools.Single(t => t.Id == "dashboard-builder").IsActive);
        Assert.Equal("Dashboard builder", shell.ActiveTitle);

        // The tool talks to the connection the host passed, not to one it opened itself: opening the
        // dashboard through it reaches the same server the shell was given.
        await dashboard.OpenAsync();
        Assert.NotNull(dashboard.Draft);
        Assert.Equal("dash-1", dashboard.Draft!.Id);

        Assert.IsType<ReportBuilderViewModel>(shell.Open("report-builder"));
        Assert.IsType<ApprovalWizardViewModel>(shell.Open("approval-wizard"));
        Assert.Equal("Approval workflow", shell.ActiveTitle);

        shell.CloseCommand.Execute(null);
        Assert.False(shell.HasActiveTool);
        Assert.All(shell.Tools, row => Assert.False(row.IsActive));
    }

    [Fact]
    public void A_registry_the_host_builds_itself_shows_only_what_it_registered()
    {
        var shell = Shell(new FakeBuilderServer(), new DesignerToolRegistry([BuilderTools.Dashboard]));

        Assert.Equal(["dashboard-builder"], shell.Tools.Select(t => t.Id));
    }
}
