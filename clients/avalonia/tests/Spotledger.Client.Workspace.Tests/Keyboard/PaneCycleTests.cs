using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Xunit;
using PaneRing = Spotledger.Client.Workspace.Keyboard.PaneRing;
using FocusTrace = Spotledger.Client.Workspace.Keyboard.FocusTrace;
using SplitView = Spotledger.Client.Workspace.Split.ListFormSplit;

namespace Spotledger.Client.Workspace.Tests.Keyboard;

/// <summary>
/// The pane ring: F6 walks the panes in order, Shift+F6 walks back, and a pane that is hidden or
/// disabled is skipped instead of focused. The chrome's panes (menu, sidebar, tabs, status) are
/// registered by the host; here they are plain controls standing in for them.
/// </summary>
public sealed class PaneCycleTests
{
    [AvaloniaFact]
    public void F6_walks_the_registered_panes_and_skips_a_hidden_one()
    {
        var menu = Named("Menu", "chrome.menu");
        var sidebar = Named("Sidebar", "chrome.sidebar");
        var tabs = Named("Tabs", "chrome.tabs");
        var list = Named("List", "split.list");
        var form = Named("Form", "split.form");
        var status = Named("Status", "chrome.status");
        sidebar.IsVisible = false;

        var ring = new PaneRing();
        ring.Add("menu", menu);
        ring.Add("sidebar", sidebar);
        ring.Add("tabs", tabs);
        ring.Add("list", list);
        ring.Add("form", form);
        ring.Add("status", status);
        var window = TestHost.Show(new StackPanel { Children = { menu, sidebar, tabs, list, form, status } });

        Assert.True(ring.Focus("menu"));
        Assert.Equal("chrome.menu", AutomationId(window.FocusManager));
        Assert.True(ring.Cycle(1));
        Assert.Equal("chrome.tabs", AutomationId(window.FocusManager));
        Assert.True(ring.Cycle(1));
        Assert.Equal("split.list", AutomationId(window.FocusManager));
        Assert.True(ring.Cycle(1));
        Assert.Equal("split.form", AutomationId(window.FocusManager));
        Assert.True(ring.Cycle(1));
        Assert.Equal("chrome.status", AutomationId(window.FocusManager));
        Assert.True(ring.Cycle(1));
        Assert.Equal("chrome.menu", AutomationId(window.FocusManager));
        window.Close();
    }

    [AvaloniaFact]
    public void Shift_F6_walks_back_and_a_disabled_pane_is_skipped()
    {
        var menu = Named("Menu", "chrome.menu");
        var tabs = Named("Tabs", "chrome.tabs");
        var list = Named("List", "split.list");
        tabs.IsEnabled = false;

        var ring = new PaneRing();
        ring.Add("menu", menu);
        ring.Add("tabs", tabs);
        ring.Add("list", list);
        var window = TestHost.Show(new StackPanel { Children = { menu, tabs, list } });

        Assert.True(ring.Focus("menu"));
        Assert.True(ring.Cycle(-1));
        Assert.Equal("split.list", AutomationId(window.FocusManager));
        Assert.True(ring.Cycle(1));
        Assert.Equal("chrome.menu", AutomationId(window.FocusManager));
        window.Close();
    }

    [AvaloniaFact]
    public async Task The_arrangement_cycles_its_own_panes_with_F6()
    {
        using var rig = new Split.SplitRig(rows: 2);
        await rig.OpenAsync("T-1");
        Assert.Equal("field:title", rig.Trace.Current);

        rig.Press(Key.F6);
        Assert.Equal("split.list", rig.Trace.Current);

        rig.Press(Key.F6, KeyModifiers.Shift);
        Assert.Equal("field:title", rig.Trace.Current);
    }

    private static TextBox Named(string name, string automationId)
    {
        var box = new TextBox { Name = name, Text = name };
        Avalonia.Automation.AutomationProperties.SetAutomationId(box, automationId);
        return box;
    }

    private static string? AutomationId(Avalonia.Input.IFocusManager? manager)
        => FocusTrace.IdOf(manager?.GetFocusedElement() as Avalonia.Visual);
}
