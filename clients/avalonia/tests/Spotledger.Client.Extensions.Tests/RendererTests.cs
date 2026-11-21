using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.VisualTree;
using Spotledger.Client.Metadata;
using Avalonia.Threading;
using Spotledger.Client.Extensions;
using Spotledger.Client.Extensions.Renderer;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>The renderer surfaces: what they draw, the tab order and accessible names they
/// expose, the RTL mirroring, and that a label in either script fits at the narrowest width.</summary>
public sealed class RendererTests
{
    [AvaloniaFact]
    public void TheActionBarDrawsOneButtonPerVisibleActionInTabOrder()
    {
        FakeDispatcher dispatcher = new();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ActionBar bar = new();
        bar.Show(view);
        Window window = TestHost.Show(bar, 420, 120);
        Assert.Equal(["Start work", "Time entries"], bar.AccessibleNames);
        List<Spotledger.Client.Controls.SpotButton> buttons = [.. Descendants(bar).OfType<Spotledger.Client.Controls.SpotButton>()];
        Assert.Equal(2, buttons.Count);
        Assert.Equal([0, 1], buttons.Select(button => button.TabIndex));
        Assert.All(buttons, button => Assert.True(button.IsVisible));
        window.Close();
    }

    [AvaloniaFact]
    public void ClickingAToolbarButtonSendsExactlyOneRequest()
    {
        FakeDispatcher dispatcher = new();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ActionBar bar = new();
        bar.Show(view);
        Window window = TestHost.Show(bar, 420, 120);
        Spotledger.Client.Controls.SpotButton start = Descendants(bar).OfType<Spotledger.Client.Controls.SpotButton>()
            .Single(button => AutomationProperties.GetName(button) == "Start work");
        Invoke(start);
        Assert.Single(dispatcher.Requests);
        Assert.Equal("start_work", dispatcher.Requests[0].Method);
        window.Close();
    }

    [AvaloniaFact]
    public void ADisabledActionIsDrawnDisabledAndAHiddenOneIsNotDrawn()
    {
        DocumentDraft draft = Sample.TaskDraft();
        draft.Set("assignee", new Spotledger.Client.Metadata.LinkFieldValue(null, null));
        FakeDispatcher dispatcher = new();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext(draft);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ActionBar bar = new();
        bar.Show(view);
        Window window = TestHost.Show(bar, 420, 120);
        Spotledger.Client.Controls.SpotButton start = Descendants(bar).OfType<Spotledger.Client.Controls.SpotButton>()
            .Single(button => AutomationProperties.GetName(button) == "Start work");
        Assert.False(start.IsEnabled);
        draft.Set("status", new TextFieldValue("Closed"));
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(["Time entries"], bar.AccessibleNames);
        window.Close();
    }

    [AvaloniaFact]
    public void TheActionBarMirrorsInRightToLeft()
    {
        // Avalonia applies the direction mirroring while rendering, not to Bounds, so the
        // proof is the frame: the RTL rasterisation differs from the LTR one.
        byte[] ltr = Frame(rtl: false);
        byte[] rtl = Frame(rtl: true);
        Assert.NotEqual(ltr, rtl);
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        ActionBar bar = new();
        bar.Show(view);
        Window window = TestHost.Show(bar, 260, 120, rtl: true);
        Assert.Equal(FlowDirection.RightToLeft, Descendants(bar).OfType<Spotledger.Client.Controls.SpotButton>().First().FlowDirection);
        window.Close();
    }

    private static byte[] Frame(bool rtl)
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        ActionBar bar = new();
        bar.Show(view);
        Window window = TestHost.Show(bar, 260, 120, rtl: rtl);
        try
        {
            using var bitmap = Spotledger.Client.TestSupport.ScreenshotAssert.Capture(window);
            using MemoryStream stream = new();
            bitmap.Save(stream, new Avalonia.Media.Imaging.PngBitmapEncoderOptions());
            return stream.ToArray();
        }
        finally
        {
            window.Close();
        }
    }

    [AvaloniaTheory]
    [InlineData("Start work on this task now")]
    [InlineData("کام شروع کریں ابھی")]
    public void ALabelInEitherScriptFitsAtTheNarrowestWidth(string label)
    {
        string document = $$"""
        {
          "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample",
          "actions": [
            { "id": "sample.long", "origin": "app:sample", "label": {{System.Text.Json.JsonSerializer.Serialize(label)}},
              "slot": "shell.menubar", "call": { "doctype": "task", "method": "start_work" } }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Sample.Facts);
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.ShellMenubar, context);
        ActionBar bar = new();
        bar.Show(view);
        Window window = TestHost.Show(bar, 200, 160);
        Spotledger.Client.Controls.SpotButton button = Descendants(bar).OfType<Spotledger.Client.Controls.SpotButton>().Single();
        TextBlock text = Descendants(button).OfType<TextBlock>().Single();
        Assert.Equal(Avalonia.Media.TextWrapping.Wrap, text.TextWrapping);
        Assert.True(button.Bounds.Width <= 200, $"button width {button.Bounds.Width} exceeds the narrowest width");
        Assert.True(text.DesiredSize.Width <= 200, $"label width {text.DesiredSize.Width} is clipped");
        Assert.True(text.Bounds.Height > 0);
        window.Close();
    }

    [AvaloniaFact]
    public void TheSidePanelDrawsTheFieldListStatTableAndText()
    {
        FakeQueries queries = FakeQueries.Sample();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocSidepanel, context, "project", new ExtensionServices(Shell: shell, Queries: queries));
        SidePanelHost host = new();
        host.Show(view);
        Window window = TestHost.Show(host, 420, 640);
        PanelModel panel = (PanelModel)view.Entries[0];
        panel.RefreshAsync(TestContext.Current.CancellationToken).GetAwaiter().GetResult();
        Dispatcher.UIThread.RunJobs();
        string[] texts = [.. Descendants(host).OfType<TextBlock>().Select(text => text.Text ?? string.Empty)];
        Assert.Contains("Overview", texts);
        Assert.Contains("Sara Ahmed", texts);
        Assert.Contains("125000.50", texts);
        Assert.Contains("Open tasks", texts);
        Assert.Contains("7", texts);
        Assert.Contains("Ali Khan", texts);
        Assert.Contains(texts, text => text.Contains("task list", StringComparison.Ordinal));
        window.Close();
    }

    [AvaloniaFact]
    public void AFailedPanelQueryDrawsItsProblemAndARetryButton()
    {
        FakeQueries queries = FakeQueries.Sample();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocSidepanel, context, "project", new ExtensionServices(Shell: shell, Queries: queries));
        queries.Throw = new InvalidOperationException("permission denied");
        SidePanelHost host = new();
        host.Show(view);
        Window window = TestHost.Show(host, 420, 640);
        PanelModel panel = (PanelModel)view.Entries[0];
        panel.RefreshAsync(TestContext.Current.CancellationToken).GetAwaiter().GetResult();
        Dispatcher.UIThread.RunJobs();
        Assert.Contains(Descendants(host).OfType<TextBlock>(), text => text.Text == "permission denied");
        Spotledger.Client.Controls.SpotButton retry = Descendants(host).OfType<Spotledger.Client.Controls.SpotButton>()
            .First(button => (AutomationProperties.GetName(button) ?? string.Empty).StartsWith("Retry", StringComparison.Ordinal));
        queries.Throw = null;
        Invoke(retry);
        Assert.Equal(PanelDataState.Ready, panel.Blocks.OfType<StatBlockView>().Single().State);
        window.Close();
    }

    [AvaloniaFact]
    public void TheTileHostDrawsTheCompositeAndOpensItsDestination()
    {
        FakeQueries queries = FakeQueries.Sample();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.HomeTiles, context, null, new ExtensionServices(Shell: shell, Queries: queries));
        TileHost host = new();
        host.Show(view);
        Window window = TestHost.Show(host, 480, 320);
        TileModel tile = (TileModel)view.Entries[0];
        tile.LoadAsync(TestContext.Current.CancellationToken).GetAwaiter().GetResult();
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(PanelDataState.Ready, tile.State);
        Assert.NotNull(tile.Root);
        Assert.Contains(tile.Root!.Children, node => node.Kind == ExpandedNodeKind.Progress);
        string[] texts = [.. Descendants(host).OfType<TextBlock>().Select(text => text.Text ?? string.Empty)];
        Assert.Contains("Project health", texts);
        Assert.Contains("7", texts);
        ComponentPresenter presenter = Descendants(host).OfType<ComponentPresenter>().Single();
        Assert.NotNull(presenter.Root);
        StackPanel stack = Assert.IsType<StackPanel>(presenter.Content);
        Assert.Contains(stack.Children, child => child is TextBlock);
        // The progress node draws a themed track with a brand fill, not a bare framework control.
        Border bar = stack.Children.OfType<Border>().Single(border => border.Child is Avalonia.Controls.Grid);
        Assert.Equal(4d, bar.Bounds.Height);
        Assert.True(bar.Bounds.Width > 0);
        Assert.IsType<SolidColorBrush>(bar.Background);
        Spotledger.Client.Controls.SpotButton button = Descendants(host).OfType<Spotledger.Client.Controls.SpotButton>()
            .Single(item => AutomationProperties.GetName(item) == "Project health");
        Invoke(button);
        Assert.Contains("open:sample.projects:", shell.Calls);
        window.Close();
    }

    [AvaloniaFact]
    public void RegisteringTheSlotsGivesTheShellOneHostPerPopulatedSlot()
    {
        FakeRegistry registry = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        FakeQueries queries = FakeQueries.Sample();
        IReadOnlyList<string> slots = ExtensionSlots.Register(registry, set, context, new ExtensionServices(new FakeDispatcher(), new FakeShell(), queries));
        Assert.Equal(["doc.sidepanel", "doc.toolbar", "home.tiles"], slots);
        Assert.Equal(["doc.sidepanel", "doc.toolbar", "home.tiles"], registry.Factories.Keys.Order(StringComparer.Ordinal));
        Assert.IsType<ActionBar>(registry.Factories["doc.toolbar"]("task"));
        Assert.IsType<SidePanelHost>(registry.Factories["doc.sidepanel"]("project"));
        Assert.IsType<TileHost>(registry.Factories["home.tiles"](null));
        Assert.Null(registry.Factories["doc.toolbar"](null));
        Assert.Empty(registry.Notices);
    }

    [AvaloniaFact]
    public void ARefusedDocumentRegistersNothingAndShowsOneNotice()
    {
        FakeRegistry registry = new();
        ExtensionSet set = ExtensionSet.Parse("""{ "contract_version": "9.0.0", "requires_shell": "0.1.0", "app": "sample" }""");
        using ExtensionContext context = Sample.ProjectContext();
        IReadOnlyList<string> slots = ExtensionSlots.Register(registry, set, context, ExtensionServices.None);
        Assert.Empty(slots);
        Assert.Empty(registry.Factories);
        (string code, string message) = Assert.Single(registry.Notices);
        Assert.Equal(ExtensionCodes.UnsupportedContract, code);
        Assert.Contains("9.0.0", message, StringComparison.Ordinal);
    }

    private static void Invoke(Control control)
    {
        ButtonAutomationPeer peer = new((Button)control);
        ((IInvokeProvider)peer).Invoke();
        Dispatcher.UIThread.RunJobs();
    }

    private static IEnumerable<Visual> Descendants(Visual root)
    {
        foreach (Visual child in root.GetVisualChildren())
        {
            yield return child;
            foreach (Visual descendant in Descendants(child))
            {
                yield return descendant;
            }
        }
    }
}
