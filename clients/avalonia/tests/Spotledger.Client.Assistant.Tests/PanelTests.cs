using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.VisualTree;
using Xunit;

namespace Spotledger.Client.Assistant.Tests;

public sealed class PanelTests
{
    private static (Window Window, AssistantPanel Panel, AssistantViewModel Vm, FakeServer Server, List<string> Opened) Show()
    {
        var server = new FakeServer();
        var vm = new AssistantViewModel(server, () => 0, d => d.StartsWith("spot://doc/", StringComparison.Ordinal));
        var opened = new List<string>();
        var panel = new AssistantPanel(vm, opened.Add);
        var window = new Window { Width = 480, Height = 640, Content = panel };
        panel.RegisterGlobalToggle(window);
        window.Show();
        return (window, panel, vm, server, opened);
    }

    private static void Settle(Window window)
    {
        Avalonia.Threading.Dispatcher.UIThread.RunJobs();
        window.UpdateLayout();
    }

    [AvaloniaFact]
    public void KeyboardToggleAndEnterToSendWorkWithoutAPointer()
    {
        var (window, panel, vm, server, _) = Show();
        server.Script.Add(new TextDelta("Hello"));
        Assert.False(panel.IsVisible);
        window.KeyPressQwerty(PhysicalKey.A, RawInputModifiers.Control | RawInputModifiers.Shift);
        Assert.True(vm.IsOpen);
        Assert.True(panel.IsVisible);

        panel.InputBox.Focus();
        panel.InputBox.Text = "hi";
        window.KeyPressQwerty(PhysicalKey.Enter, RawInputModifiers.None);
        Assert.True(vm.Transcript.OfType<UserEntry>().Any());
        window.Close();
    }

    [AvaloniaFact]
    public void EveryInteractiveControlHasAnAutomationName()
    {
        var (window, panel, vm, _, _) = Show();
        vm.SetContext("Home", "DOC-1", null, null);
        foreach (var control in panel.GetVisualDescendants().OfType<Control>().Where(c => c is Button or TextBox))
        {
            Assert.False(string.IsNullOrWhiteSpace(Avalonia.Automation.AutomationProperties.GetName(control)), control.GetType().Name);
        }

        window.Close();
    }

    [AvaloniaFact]
    public async Task RenderedTextHasNoImageAndUnregisteredLinksArePlain()
    {
        var (window, panel, vm, server, _) = Show();
        server.Script.Add(new TextDelta("![x](https://evil.example/p.png) [bad](https://evil.example) [ok](spot://doc/job/1)"));
        vm.IsOpen = true;
        vm.Input = "q";
        await vm.SendAsync(CancellationToken.None);
        Settle(window);
        Assert.Empty(panel.GetVisualDescendants().OfType<Image>());
        var buttons = panel.GetVisualDescendants().OfType<TextBlock>()
            .SelectMany(t => t.Inlines ?? [])
            .OfType<Avalonia.Controls.Documents.InlineUIContainer>()
            .Select(c => (c.Child as Button)?.Content?.ToString())
            .ToList();
        Assert.Contains("ok", buttons);
        Assert.DoesNotContain("bad", buttons);
        window.Close();
    }

    [AvaloniaFact]
    public async Task RegisteredEvidenceLinkOpensTheDocument()
    {
        var (window, panel, vm, server, opened) = Show();
        server.Script.Add(new ToolEvent(new ToolActivity("looked at job queue", [new EvidenceRef("job 17", "spot://doc/job/17")])));
        vm.IsOpen = true;
        vm.Input = "q";
        await vm.SendAsync(CancellationToken.None);
        Settle(window);
        var toggle = panel.GetVisualDescendants().OfType<Button>().First(b => (b.Content?.ToString() ?? string.Empty).Contains("looked at job queue", StringComparison.Ordinal));
        toggle.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));
        Settle(window);
        var link = panel.GetVisualDescendants().OfType<Button>().First(b => b.Content?.ToString() == "job 17");
        link.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));
        Assert.Equal(["spot://doc/job/17"], opened);
        window.Close();
    }
}
