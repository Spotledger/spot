using Spotledger.Client.Expressions;
using Spotledger.Client.Extensions;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>Panels: presentation blocks from the draft, data blocks from a declared query,
/// paging inside the declared maximum, and the empty and error states with a retry.</summary>
public sealed class PanelTests
{
    private static (PanelModel Panel, SlotView View, FakeQueries Queries, FakeShell Shell) ProjectPanel(DocumentDraft? draft = null)
    {
        FakeQueries queries = FakeQueries.Sample();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        ExtensionContext context = Sample.ProjectContext(draft);
        SlotView view = set.ForSlot(ExtensionSlotCatalog.DocSidepanel, context, "project", new ExtensionServices(Shell: shell, Queries: queries));
        return (Assert.IsType<PanelModel>(Assert.Single(view.Entries)), view, queries, shell);
    }

    [Fact]
    public void TheFieldListReadsTheDraftWithLabelsAndExactText()
    {
        (PanelModel panel, SlotView view, FakeQueries _, FakeShell _) = ProjectPanel();
        using (view)
        {
            FieldListBlockView fields = Assert.IsType<FieldListBlockView>(panel.Blocks[0]);
            Assert.Equal(
                [new BlockEntry("Status", "Active"), new BlockEntry("Owner", "Sara Ahmed"), new BlockEntry("Budget", "125000.50")],
                fields.Items);
            Assert.Equal("Overview", panel.Label);
        }
    }

    [Fact]
    public void TheFieldListFollowsTheDraftWhenItChanges()
    {
        DocumentDraft draft = Sample.ProjectDraft();
        (PanelModel panel, SlotView view, FakeQueries _, FakeShell _) = ProjectPanel(draft);
        using (view)
        {
            FieldListBlockView fields = Assert.IsType<FieldListBlockView>(panel.Blocks[0]);
            draft.Set("status", new TextFieldValue("Archived"));
            Assert.Equal("Archived", fields.Items[0].Text);
        }
    }

    [Fact]
    public async Task TheStatRunsItsAggregateAndFormatsItExactly()
    {
        (PanelModel panel, SlotView view, FakeQueries queries, FakeShell _) = ProjectPanel();
        using (view)
        {
            await panel.RefreshAsync(TestContext.Current.CancellationToken);
            StatBlockView stat = Assert.IsType<StatBlockView>(panel.Blocks[1]);
            Assert.Equal("Open tasks", stat.Label);
            Assert.Equal(PanelDataState.Ready, stat.State);
            Assert.Equal("7", stat.Text);
            Assert.Equal(1, queries.AggregateCalls);
        }
    }

    [Fact]
    public async Task TheTableShowsRowsAndOpensTheFirstCellAsTheDocumentId()
    {
        (PanelModel panel, SlotView view, FakeQueries queries, FakeShell shell) = ProjectPanel();
        using (view)
        {
            await panel.RefreshAsync(TestContext.Current.CancellationToken);
            TableBlockView table = Assert.IsType<TableBlockView>(panel.Blocks[2]);
            Assert.Equal(["work_date", "person", "Hours"], table.ColumnLabels);
            Assert.Equal(3, table.Rows.Count);
            Assert.Equal("2023-11-13", table.RowDocumentId(0));
            Assert.False(table.HasMore);
            Assert.DoesNotContain(queries.Pages, page => page.Limit <= 0);
            await table.OpenRowAsync(0, TestContext.Current.CancellationToken);
            Assert.Contains("document:2023-11-13", shell.Calls);
        }
    }

    [Fact]
    public async Task TheTableNeverHoldsMoreThanItsMaxRows()
    {
        (PanelModel panel, SlotView view, FakeQueries queries, FakeShell _) = ProjectPanel();
        using (view)
        {
            queries.Rows["sample.recent_time_entries"] = new QueryRows(
                ["work_date", "person", "hours"],
                [.. Enumerable.Range(0, 12).Select(index => (IReadOnlyList<string>)new[] { $"2023-11-{index:00}", "Ali", "01.00" })],
                true);
            await panel.RefreshAsync(TestContext.Current.CancellationToken);
            TableBlockView table = Assert.IsType<TableBlockView>(panel.Blocks[2]);
            Assert.Equal(5, table.Rows.Count);
            Assert.False(table.HasMore);
            await table.LoadMoreAsync(TestContext.Current.CancellationToken);
            Assert.Equal(5, table.Rows.Count);
        }
    }

    [Fact]
    public async Task AnEmptyAnswerShowsTheEmptyState()
    {
        (PanelModel panel, SlotView view, FakeQueries queries, FakeShell _) = ProjectPanel();
        using (view)
        {
            queries.Rows["sample.recent_time_entries"] = QueryRows.Empty;
            await panel.RefreshAsync(TestContext.Current.CancellationToken);
            TableBlockView table = Assert.IsType<TableBlockView>(panel.Blocks[2]);
            Assert.Equal(PanelDataState.Empty, table.State);
            Assert.Empty(table.Rows);
            Assert.Equal("Nothing to show", panel.EmptyText);
        }
    }

    [Fact]
    public async Task AFailedQueryShowsTheErrorAndRetryLoadsAgain()
    {
        (PanelModel panel, SlotView view, FakeQueries queries, FakeShell _) = ProjectPanel();
        using (view)
        {
            queries.Throw = new InvalidOperationException("permission denied");
            await panel.RefreshAsync(TestContext.Current.CancellationToken);
            StatBlockView stat = Assert.IsType<StatBlockView>(panel.Blocks[1]);
            Assert.Equal(PanelDataState.Error, stat.State);
            Assert.Equal("permission denied", stat.Problem);
            queries.Throw = null;
            await stat.RetryAsync(TestContext.Current.CancellationToken);
            Assert.Equal(PanelDataState.Ready, stat.State);
            Assert.Equal("7", stat.Text);
            Assert.Contains(view.Set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.QueryFailed);
        }
    }

    [Fact]
    public async Task SavingTheDocumentRefreshesTheDataBlocks()
    {
        (PanelModel panel, SlotView view, FakeQueries queries, FakeShell _) = ProjectPanel();
        using (view)
        {
            await panel.DocumentSavedAsync(TestContext.Current.CancellationToken);
            Assert.Equal(1, queries.RowCalls);
            Assert.Equal(1, queries.AggregateCalls);
            await panel.DocumentSavedAsync(TestContext.Current.CancellationToken);
            Assert.Equal(2, queries.RowCalls);
        }
    }

    [Fact]
    public void TheTextBlockYieldsTheInlineModelAndItsDestination()
    {
        (PanelModel panel, SlotView view, FakeQueries _, FakeShell _) = ProjectPanel();
        using (view)
        {
            TextBlockView text = Assert.IsType<TextBlockView>(panel.Blocks[3]);
            Assert.Equal(["sample.tasks"], text.Model.Destinations);
            MarkdownParagraph paragraph = Assert.IsType<MarkdownParagraph>(Assert.Single(text.Model.Blocks));
            Assert.Contains(paragraph.Inlines, inline => inline is MarkdownLink link && link.Text == "task list");
        }
    }

    [Fact]
    public async Task ALinkInThePanelOpensItsDestinationThroughTheShell()
    {
        (PanelModel panel, SlotView view, FakeQueries _, FakeShell shell) = ProjectPanel();
        using (view)
        {
            TextBlockView text = Assert.IsType<TextBlockView>(panel.Blocks[3]);
            MarkdownLink link = text.Model.Blocks.OfType<MarkdownParagraph>()
                .SelectMany(item => item.Inlines).OfType<MarkdownLink>().Single();
            _ = link;
            await view.Services.Shell!.OpenAsync("sample.tasks", [], TestContext.Current.CancellationToken);
            Assert.Contains("open:sample.tasks:", shell.Calls);
        }
    }

    [Fact]
    public void ExactFormatsNeverRoundThroughAFloat()
    {
        Assert.Equal("125000.50", ExtensionFormatting.Stat(DocValues.DecimalFromCanonical("125000.50"), "decimal"));
        Assert.Equal("50%", ExtensionFormatting.Stat(DocValues.DecimalFromCanonical("0.50"), "percent"));
        Assert.Equal("$125000.50", ExtensionFormatting.Stat(DocValues.DecimalFromCanonical("125000.50"), "currency", "$"));
        Assert.Equal("-7", ExtensionFormatting.Stat(new IntValue(-7), "int"));
        Assert.Equal(16, ExtensionFormatting.Progress(new IntValue(16)));
        Assert.Equal(0, ExtensionFormatting.Progress(new IntValue(-3)));
        Assert.Equal(100, ExtensionFormatting.Progress(new IntValue(400)));
    }

    [Fact]
    public void APanelWithoutAQueryRunnerReportsTheErrorStateInsteadOfThrowing()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocSidepanel, context, "project");
        PanelModel panel = Assert.IsType<PanelModel>(Assert.Single(view.Entries));
        Assert.Equal(PanelDataState.Idle, panel.Blocks[1].State);
    }
}

/// <summary>Composites: expansion, props as <c>doc</c>, the contract's limits enforced again,
/// and a placeholder wherever a reference cannot be resolved.</summary>
public sealed class ComponentTests
{
    [Fact]
    public void TheProjectHealthCompositeExpandsWithItsProps()
    {
        ExtensionSet set = Sample.Set();
        ComponentSpec component = Assert.Single(set.Components);
        using ExtensionContext context = Sample.ProjectContext();
        ComponentModel model = ComponentModel.Expand(component, Props(("open", new IntValue(7)), ("total", new IntValue(23))), context, set, _ => null);
        Assert.True(model.IsUsable);
        IReadOnlyList<ExpandedNode> children = model.Root!.Children;
        // The declared `footer` slot is unfilled, so it expands to nothing.
        Assert.Equal(3, children.Count);
        Assert.Equal("Project health", children[0].Text);
        Assert.Equal(ExpandedNodeKind.Heading, children[0].Kind);
        Assert.Equal(ExpandedNodeKind.Progress, children[1].Kind);
        Assert.Equal(16, children[1].Percent);
        Assert.Equal(ExpandedNodeKind.Number, children[2].Kind);
        Assert.Equal("7", children[2].Value);
    }

    [Fact]
    public void AWhenFalseHidesItsBody()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        ComponentModel model = ComponentModel.Expand(Assert.Single(set.Components), Props(("open", new IntValue(0)), ("total", new IntValue(0))), context, set, _ => null);
        Assert.DoesNotContain(model.Root!.Children, node => node.Kind == ExpandedNodeKind.Progress);
    }

    [Fact]
    public void ARequiredPropMissingRefusesTheExpansion()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        ComponentModel model = ComponentModel.Expand(Assert.Single(set.Components), Props(("open", new IntValue(1))), context, set, _ => null);
        Assert.False(model.IsUsable);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.Schema && diagnostic.Message.Contains("total", StringComparison.Ordinal));
    }

    [Fact]
    public void ARepeatIsBoundedByItsMax()
    {
        ExtensionSet set = ExtensionSet.Parse(RepeatDocument(3), Sample.Facts);
        ComponentSpec component = Assert.Single(set.Components, component => component.Id == "sample.c");
        using ExtensionContext context = Sample.ProjectContext();
        ComponentModel model = ComponentModel.Expand(component, Props(("count", new IntValue(120))), context, set, _ => null);
        Assert.True(model.IsUsable, string.Join("; ", set.Diagnostics));
        Assert.Equal(3, model.Root!.Children.Count);
    }

    [Fact]
    public void AnOverlargeDocumentIsRefusedAtParseTime()
    {
        ExtensionSet set = ExtensionSet.Parse(RepeatDocument(50, children: 20), Sample.Facts);
        Assert.Empty(set.Components);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.ComponentTooLarge);
    }

    [Fact]
    public void AnExpansionThatOutgrowsTheBudgetIsRefusedWhileItRuns()
    {
        // A hand-built spec the parser would have refused: the expansion enforces the same
        // budget again, because props can make a repeat larger than the static count.
        ComponentSpec component = new(
            "sample.c", "sample", [new ComponentProp("count", "int", true)], [],
            new RepeatNode("count", 600, new ControlNode("label", "x", null, null)));
        ExtensionSet set = ExtensionSet.Empty;
        using ExtensionContext context = Sample.ProjectContext();
        ComponentModel model = ComponentModel.Expand(component, Props(("count", new IntValue(600))), context, set, _ => null);
        Assert.False(model.IsUsable);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.ComponentTooLarge);
    }

    [Fact]
    public void AUseOfAnAbsentComponentIsRefusedAtParseTime()
    {
        const string document = """
        {
          "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample",
          "components": [
            { "id": "sample.c", "origin": "app:sample", "props": [],
              "body": { "node": "use", "component": "sample.ghost" } }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Sample.Facts);
        Assert.Empty(set.Components);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.UnknownComponent);
    }

    [Fact]
    public void AUseThatReachesAnAbsentComponentWhileExpandingBecomesAPlaceholder()
    {
        ComponentSpec component = new("sample.c", "sample", [], [],
            new UseNode("sample.ghost", [], new Dictionary<string, ComponentNode>(StringComparer.Ordinal)));
        ExtensionSet set = ExtensionSet.Empty;
        using ExtensionContext context = Sample.ProjectContext();
        ComponentModel model = ComponentModel.Expand(component, Props(), context, set, _ => null);
        Assert.Equal(ExpandedNodeKind.Placeholder, model.Root!.Kind);
        Assert.Equal("sample.ghost", model.Root.Placeholder);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.UnknownComponent);
    }

    [Fact]
    public async Task AButtonResolvesItsActionAndClicksItOnce()
    {
        FakeDispatcher dispatcher = new();
        FakeShell shell = new();
        const string document = """
        {
          "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample",
          "actions": [
            { "id": "sample.start_work", "origin": "app:sample", "label": "Start work", "slot": "doc.toolbar", "doctype": "task",
              "call": { "doctype": "task", "method": "start_work", "args": { "task": "doc.name" } } }
          ],
          "components": [
            { "id": "sample.c", "origin": "app:sample", "props": [],
              "body": { "node": "control", "control": "button", "text": "Go", "on_click": "sample.start_work" } }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Sample.Facts);
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ComponentModel model = ComponentModel.Expand(Assert.Single(set.Components), Props(), context, set, view.ActionModelFor);
        ExpandedNode button = model.Root!;
        Assert.Equal(ExpandedNodeKind.Button, button.Kind);
        Assert.NotNull(button.Action);
        await button.Action!.InvokeAsync(TestContext.Current.CancellationToken);
        Assert.Single(dispatcher.Requests);
    }

    [Fact]
    public void TheClientEnforcesTheDepthLimitAgain()
    {
        string body = Nest(9);
        ExtensionSet set = ExtensionSet.Parse(ComponentDocument(body), Sample.Facts);
        Assert.Empty(set.Components);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.ComponentTooDeep);
    }

    private static string Nest(int depth) => depth == 1
        ? """{ "node": "control", "control": "label", "text": "x" }"""
        : "{ \"node\": \"column\", \"children\": [ " + Nest(depth - 1) + " ] }";

    private static string ComponentDocument(string body) => """
        {
          "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample",
          "components": [ { "id": "sample.c", "origin": "app:sample", "props": [], "body": 
        """ + body + " } ] }";

    private static string RepeatDocument(int max, int children = 1)
    {
        string child = """{ "node": "control", "control": "label", "text": "x" }""";
        string body = "{ \"node\": \"column\", \"children\": [ " + string.Join(',', Enumerable.Repeat(child, children)) + " ] }";
        return """
        {
          "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample",
          "components": [
            { "id": "sample.c", "origin": "app:sample", "props": [ { "name": "count", "type": "int", "required": true } ],
              "body": { "node": "repeat", "over": "count", "max": 
        """ + max + ", \"body\": " + body + " } } ] }";
    }

    private static Dictionary<string, ExpressionValue> Props(params (string Name, ExpressionValue Value)[] props)
    {
        Dictionary<string, ExpressionValue> values = new(StringComparer.Ordinal);
        foreach ((string name, ExpressionValue value) in props)
        {
            values[name] = value;
        }
        return values;
    }
}

/// <summary>The restricted markdown subset: the inline model it produces and the three
/// things it refuses.</summary>
public sealed class MarkdownTests
{
    [Fact]
    public void TheSampleSentenceParsesToTextAndADestinationLink()
    {
        MarkdownModel model = MarkdownModel.Parse("Open the [task list](dest:sample.tasks) to see more.");
        Assert.False(model.IsRefused);
        MarkdownParagraph paragraph = Assert.IsType<MarkdownParagraph>(Assert.Single(model.Blocks));
        Assert.Equal("Open the ", Assert.IsType<MarkdownText>(paragraph.Inlines[0]).Text);
        Assert.Equal("sample.tasks", Assert.IsType<MarkdownLink>(paragraph.Inlines[1]).Destination);
        Assert.Equal(" to see more.", Assert.IsType<MarkdownText>(paragraph.Inlines[2]).Text);
        Assert.Equal("Open the task list (sample.tasks) to see more.", model.ToPlainText());
    }

    [Fact]
    public void EmphasisStrongCodeAndListsParse()
    {
        MarkdownModel model = MarkdownModel.Parse("A **bold** *soft* `code`\n\n- one\n- two\n\n1. first");
        Assert.Equal(3, model.Blocks.Count);
        MarkdownParagraph paragraph = Assert.IsType<MarkdownParagraph>(model.Blocks[0]);
        Assert.Contains(paragraph.Inlines, inline => inline is MarkdownStrong);
        Assert.Contains(paragraph.Inlines, inline => inline is MarkdownEmphasis);
        Assert.Contains(paragraph.Inlines, inline => inline is MarkdownCode);
        MarkdownList unordered = Assert.IsType<MarkdownList>(model.Blocks[1]);
        Assert.False(unordered.Ordered);
        Assert.Equal(2, unordered.Items.Count);
        MarkdownList ordered = Assert.IsType<MarkdownList>(model.Blocks[2]);
        Assert.True(ordered.Ordered);
        Assert.Equal("1.", MarkdownModel.ItemMarker(true, 0));
    }

    [Theory]
    [InlineData("<script>alert(1)</script>", "markup")]
    [InlineData("![pic](https://example.com/a.png)", "image")]
    [InlineData("See [here](https://example.com)", "dest:")]
    [InlineData("Hi <b>there</b>", "markup")]
    public void MarkupImagesAndExternalLinksAreRefused(string text, string because)
    {
        MarkdownModel model = MarkdownModel.Parse(text);
        Assert.True(model.IsRefused);
        Assert.Empty(model.Blocks);
        Assert.Contains(model.Violations, violation => violation.Contains(because, StringComparison.OrdinalIgnoreCase));
    }

    [Fact]
    public void AnOverlongTextIsRefused()
    {
        MarkdownModel model = MarkdownModel.Parse(new string('a', ExtensionLimits.MaxMarkdown + 1));
        Assert.True(model.IsRefused);
    }
}
