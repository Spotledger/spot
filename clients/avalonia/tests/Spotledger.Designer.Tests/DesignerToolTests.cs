using Spotledger.Client.Designer;
using Xunit;

namespace Spotledger.Designer.Tests;

/// <summary>
/// The designer shell's tool extension point: the registry the composition root fills, and the list
/// the shell shows. The shell must never need to know which tools exist, and a tool that cannot be
/// created must leave the DocType designer working with a sentence to read.
/// </summary>
public sealed class DesignerToolTests
{
    private static DesignerViewModel Designer(DesignerToolRegistry registry, DesignerToolContext? context = null)
    {
        var server = new FakeDesignerServer(FakeDesignerServer.NewVehicleDraft());
        var session = new DesignerSession(server, "draft-vsr");
        return new DesignerViewModel(session, registry, context);
    }

    [Fact]
    public void TheRegistryKeepsTheOrderTheCompositionRootRegisteredToolsIn()
    {
        var registry = new DesignerToolRegistry()
            .Register(new FakeTool("approval-wizard", "Approval workflow"))
            .Register(new FakeTool("dashboard-builder", "Dashboard builder"))
            .Register(new FakeTool("report-builder", "Report builder"));

        Assert.Equal(
            ["approval-wizard", "dashboard-builder", "report-builder"],
            registry.Tools.Select(t => t.Id));
        Assert.True(registry.Contains("report-builder"));
        Assert.Null(registry.Find("no-such-tool"));

        // The registry can also be built from a list, which keeps that list's order.
        var fromList = new DesignerToolRegistry([new FakeTool("a", "A"), new FakeTool("b", "B")]);
        Assert.Equal(["a", "b"], fromList.Tools.Select(t => t.Id));
    }

    [Fact]
    public void TheRegistryRefusesASecondToolWithTheSameId()
    {
        var registry = new DesignerToolRegistry().Register(new FakeTool("report-builder", "Report builder"));

        var refusal = Assert.Throws<ArgumentException>(
            () => registry.Register(new FakeTool("report-builder", "Report builder, again")));
        Assert.Contains("report-builder", refusal.Message, StringComparison.Ordinal);

        // The first tool still answers for the id; the duplicate changed nothing.
        Assert.Single(registry.Tools);
        Assert.Equal("Report builder", registry.Find("report-builder")!.Title);

        // An id is required too: a tool the shell cannot name can never be opened.
        Assert.ThrowsAny<ArgumentException>(() => registry.Register(new FakeTool(string.Empty, "Nameless")));
    }

    [Fact]
    public void TheShellListsEveryRegisteredToolAndOpensTheOneThatWasPressed()
    {
        var tool = new FakeTool("report-builder", "Report builder");
        var designer = Designer(new DesignerToolRegistry().Register(tool));

        Assert.True(designer.Tools.HasTools);
        var row = Assert.Single(designer.Tools.Tools);
        Assert.Equal("report-builder", row.Id);
        Assert.Equal("Report builder", row.Title);
        Assert.False(designer.Tools.HasActiveTool);

        row.OpenCommand.Execute(null);

        Assert.True(designer.Tools.HasActiveTool);
        Assert.True(row.IsActive);
        Assert.Equal("Report builder", designer.Tools.ActiveTitle);
        Assert.IsType<FakeToolViewModel>(designer.Tools.ActiveViewModel);

        // The tool is given the designer's own connection and draft, so it can reach the server the
        // designer is already using instead of opening a second one.
        Assert.Same(designer.Session.Server, tool.Seen!.Server);
        Assert.Equal("draft-vsr", tool.Seen.ItemId);

        designer.Tools.CloseCommand.Execute(null);
        Assert.False(designer.Tools.HasActiveTool);
        Assert.False(row.IsActive);
    }

    [Fact]
    public void AToolThatThrowsIsShownAsAPlainMessageAndTheDesignerKeepsWorking()
    {
        var registry = new DesignerToolRegistry()
            .Register(new FakeTool("broken", "Broken builder", _ => throw new InvalidOperationException("no connection for it")))
            .Register(new FakeTool("report-builder", "Report builder"));
        var designer = Designer(registry);

        var opened = designer.Tools.Open("broken");

        Assert.Null(opened);
        Assert.False(designer.Tools.HasActiveTool);
        Assert.Null(designer.Tools.ActiveViewModel);
        Assert.True(designer.Tools.HasMessage);
        Assert.Contains("could not be opened", designer.Tools.Message, StringComparison.Ordinal);
        Assert.Contains("no connection for it", designer.Tools.Message, StringComparison.Ordinal);
        Assert.DoesNotContain("Exception", designer.Tools.Message, StringComparison.Ordinal);

        // The other tools still open, and the failed attempt left no tool marked active.
        Assert.All(designer.Tools.Tools, row => Assert.False(row.IsActive));
        Assert.IsType<FakeToolViewModel>(designer.Tools.Open("report-builder"));
        Assert.False(designer.Tools.HasMessage);
    }

    [Fact]
    public void TheShellSaysSoForAToolItDoesNotHave()
    {
        var designer = Designer(new DesignerToolRegistry().Register(new FakeTool("report-builder", "Report builder")));

        Assert.Null(designer.Tools.Open("no-such-tool"));
        Assert.False(designer.Tools.HasActiveTool);
        Assert.Contains("no-such-tool", designer.Tools.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void ADesignerWithNoRegisteredToolsShowsNoToolStrip()
    {
        var designer = DesignerHarness.Create().Designer;

        Assert.False(designer.Tools.HasTools);
        Assert.False(designer.Tools.HasActiveTool);
        Assert.Empty(designer.Tools.Tools);
    }

    /// <summary>A tool that records the context the shell hands it, so the tests can check it.</summary>
    private sealed class FakeTool(string id, string title, Func<DesignerToolContext, object>? create = null) : IDesignerTool
    {
        public string Id => id;

        public string Title => title;

        public string Description => $"{title}: one line about it.";

        public DesignerToolContext? Seen { get; private set; }

        public object Create(DesignerToolContext context)
        {
            Seen = context;
            return create is null ? new FakeToolViewModel() : create(context);
        }
    }

    /// <summary>Stands in for a tool's own view-model: the shell shows it through the host's templates.</summary>
    private sealed class FakeToolViewModel;
}
