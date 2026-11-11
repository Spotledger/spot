using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Media;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Demo.Mock;
using Xunit;

namespace Spotledger.Demo.Tests;

public sealed class ViewTests
{
    [AvaloniaFact]
    public void DesignSystemScrollViewerCanBeUsedByTheDemo()
    {
        // Minimal dependency reproduction: no shell or view-model is involved.
        var window = new Window { Content = new ScrollViewer { Content = new TextBlock { Text = "Demo content" } } };
        try { window.Show(); TestHost.Pump(); }
        finally { window.Close(); }
    }
    [AvaloniaFact]
    public async Task AltSequenceOpensProcurementAndReportsSameRestrictionAsOtherRoutes()
    {
        var (window, view, model) = await TestHost.ShowAsync();
        try
        {
            TestHost.Key(window, Key.LeftAlt); Assert.Equal("menu", view.OverlayKind);
            TestHost.Key(window, Key.T); Assert.Equal("Transactions", view.MenuPath);
            TestHost.Key(window, Key.P); Assert.Equal("Transactions/Procurement", view.MenuPath);
            TestHost.Key(window, Key.R); Assert.Equal("information", view.OverlayKind);
            Assert.Equal("The demo Warehouse operator role is required.", model.StatusMessage);
            view.CloseOverlay(); TestHost.Pump();
            var context = model.Context;
            foreach (var route in new[] { CommandRoute.Menu, CommandRoute.Launcher, CommandRoute.Shortcut, CommandRoute.ContextMenu })
            {
                var result = await view.InvokeAsync("create.receipt", route);
                Assert.Equal(context, model.Context);
                Assert.Equal(model.StatusMessage, result.Availability.Reason);
                Assert.False(result.Executed); view.CloseOverlay(); TestHost.Pump();
            }
            await view.InvokeAsync("demo.role.WarehouseOperator", CommandRoute.Menu); TestHost.Pump();
            TestHost.Key(window, Key.LeftAlt); TestHost.Key(window, Key.T); TestHost.Key(window, Key.P); TestHost.Key(window, Key.R);
            Assert.Equal("create.receipt", model.Tabs.Active!.Key);
        }
        finally { window.Close(); }
    }
    [AvaloniaFact]
    public async Task LauncherSearchesAliasesAndRunsSelectionAndRestoresOriginFocus()
    {
        var (window, view, model) = await TestHost.ShowAsync();
        try
        {
            await view.InvokeAsync("edit.note", CommandRoute.Menu); TestHost.Pump();
            view.NoteInput!.TextBoxControl!.Focus(); TestHost.Pump();
            TestHost.Key(window, Key.K, KeyModifiers.Control); Assert.Equal("launcher", view.OverlayKind);
            await view.SearchAsync("buy materials"); TestHost.Pump();
            var list = TestHost.Find<ListBox>(view, "LauncherResults");
            Assert.Equal("create.request", Assert.IsType<ShellView.LauncherEntry>(Assert.Single(list.Items)).Definition.Id);
            TestHost.Key(window, Key.Escape); TestHost.Pump();
            Assert.True(view.NoteInput!.TextBoxControl!.IsFocused);
            TestHost.Key(window, Key.K, KeyModifiers.Control); await view.SearchAsync("buy materials");
            TestHost.Key(window, Key.Enter); TestHost.Pump();
            Assert.Equal("create.request", model.Tabs.Active!.Key);
            TestHost.Key(window, Key.K, KeyModifiers.Control); await view.SearchAsync("required");
            Assert.NotNull(TestHost.Find<ListBox>(view, "LauncherResults")); view.CloseOverlay();
        }
        finally { window.Close(); }
    }
    [AvaloniaFact]
    public async Task MenuAndDialogReturnFocusToOriginAndDirtyDialogTrapsFocus()
    {
        var (window, view, model) = await TestHost.ShowAsync();
        try
        {
            await view.InvokeAsync("edit.note", CommandRoute.Menu); TestHost.Pump();
            view.NoteInput!.TextBoxControl!.Focus(); TestHost.Pump();
            TestHost.Key(window, Key.LeftAlt); TestHost.Key(window, Key.T); TestHost.Key(window, Key.P);
            TestHost.Key(window, Key.Left); Assert.Equal("Transactions", view.MenuPath);
            TestHost.Key(window, Key.Escape); TestHost.Key(window, Key.Escape); TestHost.Pump();
            Assert.True(view.NoteInput!.TextBoxControl!.IsFocused);
            model.Tabs.Active!.Edit("Dirty note");
            var closing = view.InvokeAsync("tab.close", CommandRoute.Shortcut); TestHost.Pump();
            Assert.Equal("dirty", view.OverlayKind); Assert.True(TestHost.Find<SpotButton>(view, "DirtyCancel").IsFocused);
            TestHost.Key(window, Key.Tab); TestHost.Key(window, Key.Tab); TestHost.Key(window, Key.Tab);
            var focused = view.GetVisualDescendants().OfType<Control>().Single(c => c.IsFocused);
            Assert.StartsWith("Dirty", focused.Name);
            TestHost.Key(window, Key.Escape); await closing; TestHost.Pump();
            Assert.True(model.Tabs.Active!.IsDirty); Assert.True(view.NoteInput!.TextBoxControl!.IsFocused);
        }
        finally { window.Close(); }
    }
    [AvaloniaFact]
    public async Task ShortcutSaveAndOverflowSwitchTabsAndKeepEditorState()
    {
        var (window, view, model) = await TestHost.ShowAsync();
        try
        {
            await view.InvokeAsync("edit.note", CommandRoute.Menu); TestHost.Pump();
            var input = view.NoteInput!.TextBoxControl!; input.Focus(); input.Text = "Typed demo note"; TestHost.Pump();
            Assert.True(model.Tabs.Active!.IsDirty);
            TestHost.Key(window, Key.S, KeyModifiers.Control); TestHost.Pump();
            Assert.False(model.Tabs.Active!.IsDirty); Assert.Equal("Typed demo note", model.Tabs.Active.Record!.Note);
            TestHost.Key(window, Key.Tab, KeyModifiers.Control); Assert.Equal("home", model.Tabs.Active!.Key);
            TestHost.Key(window, Key.Tab, KeyModifiers.Control | KeyModifiers.Shift); Assert.Equal(MockBackend.RecordId.ToString(), model.Tabs.Active!.Key);
            TestHost.Key(window, Key.L, KeyModifiers.Control | KeyModifiers.Shift); Assert.Equal("tabs", view.OverlayKind);
            Assert.True(TestHost.Find<SpotButton>(view, "Overflow" + MockBackend.RecordId).IsFocused);
            TestHost.Key(window, Key.Escape); TestHost.Key(window, Key.W, KeyModifiers.Control); Assert.Single(model.Tabs.Tabs);
        }
        finally { window.Close(); }
    }
    [AvaloniaFact]
    public async Task SidebarRailSimpleHomeAndUrduUsePresentationSettings()
    {
        var (window, view, model) = await TestHost.ShowAsync();
        try
        {
            Assert.NotNull(TestHost.Find<SpotButton>(view, "Kpi0"));
            TestHost.Key(window, Key.B, KeyModifiers.Control); Assert.Equal(SidebarMode.Icons, model.Sidebar);
            TestHost.Key(window, Key.B, KeyModifiers.Control); Assert.Equal(SidebarMode.Hidden, model.Sidebar);
            Assert.DoesNotContain(view.GetVisualDescendants().OfType<SpotButton>(), b => b.Name == "Sidebarhome");
            await view.InvokeAsync("view.rail", CommandRoute.Menu); Assert.False(model.RailVisible);
            Assert.DoesNotContain(view.GetVisualDescendants().OfType<SpotButton>(), b => b.Name == "RailToggle");
            await view.InvokeAsync("demo.governance", CommandRoute.Menu); TestHost.Pump();
            Assert.DoesNotContain(view.GetVisualDescendants().OfType<SpotButton>(), b => b.Name == "Kpi0");
            Assert.Contains(view.GetVisualDescendants().OfType<SpotButton>(), b => b.Name!.StartsWith("Task", StringComparison.Ordinal));
            await view.InvokeAsync("demo.locale.ur", CommandRoute.Menu); TestHost.Pump();
            Assert.Equal(FlowDirection.RightToLeft, view.FlowDirection);
            Assert.Contains(view.GetVisualDescendants().OfType<TextBlock>(), t => t.Text!.Contains("خوش آمدید", StringComparison.Ordinal));
            await view.InvokeAsync("demo.density.compact", CommandRoute.Menu); TestHost.Pump(); Assert.Equal(DensityMode.Compact, Density.GetMode(view));
            await view.InvokeAsync("demo.maintenance", CommandRoute.Menu); TestHost.Pump(); Assert.True(model.Settings.Maintenance);
            await view.InvokeAsync("demo.enforcement.Strict", CommandRoute.Menu); TestHost.Pump(); Assert.Equal(EnforcementLevel.Strict, model.Settings.Enforcement);
        }
        finally { window.Close(); }
    }
    [AvaloniaFact]
    public async Task BrowserProfileLeavesReservedChordsAndAltAndOrdinaryTypingAlone()
    {
        var (window, view, model) = await TestHost.ShowAsync();
        try
        {
            await view.InvokeAsync("demo.keymap.browser", CommandRoute.Menu);
            await view.InvokeAsync("edit.note", CommandRoute.Menu); TestHost.Pump();
            view.NoteInput!.TextBoxControl!.Focus();
            TestHost.Key(window, Key.LeftAlt); Assert.Empty(view.OverlayKind);
            TestHost.Key(window, Key.W, KeyModifiers.Control); Assert.Equal(2, model.Tabs.Tabs.Count);
            TestHost.Key(window, Key.K, KeyModifiers.Control | KeyModifiers.Alt); Assert.Empty(view.OverlayKind);
            window.KeyTextInput("plain text"); TestHost.Pump(); Assert.True(model.Tabs.Active!.IsDirty);
            model.AskDirtyAsync = _ => Task.FromResult(DirtyChoice.Discard);
            TestHost.Key(window, Key.F4, KeyModifiers.Control | KeyModifiers.Shift); Assert.Single(model.Tabs.Tabs);
        }
        finally { window.Close(); }
    }
}
