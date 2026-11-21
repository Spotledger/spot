using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;
using Spotledger.Demo.Mock;

namespace Spotledger.Demo;

/// <summary>Token-styled renderer of demo presentation state; all actions use the dispatcher.</summary>
public sealed class ShellView : UserControl
{
    private readonly ShellViewModel model;
    private readonly Grid root = new();
    private Control main = new StackPanel();
    private Border? overlay;
    private string? originName;
    private string? lastFocusName;
    private Control? lastFocusControl;
    private Control? originControl;
    private string layer = "";
    private string menuPath = "";
    private int selectedIndex;
    private IReadOnlyList<(string Label, char Mnemonic, string? CommandId, string? Path)> menuItems = [];
    private ListBox? launcherList;
    private List<LauncherEntry> launcherEntries = [];
    private TaskCompletionSource<DirtyChoice>? dirtyCompletion;
    private ScrollViewer? contentScroll;
    private DemoTab? renderedTab;
    public SpotButton LauncherButton { get; private set; } = null!;
    public SpotTextInput? NoteInput { get; private set; }
    public string OverlayKind => layer;
    public string MenuPath => menuPath;
    public ShellViewModel Model => model;
    public ShellView(ShellViewModel model)
    {
        this.model = model;
        DataContext = model;
        Content = root;
        model.PropertyChanged += (_, _) => Render();
        model.LauncherRequested += ShowLauncher;
        model.OverflowRequested += ShowTabs;
        model.InformationRequested += ShowInformation;
        model.AskDirtyAsync = ShowDirtyAsync;
        AddHandler(KeyDownEvent, HandleKeyDown, RoutingStrategies.Tunnel);
        Render();
    }
    private static object Resource(string key)
    {
        if (Application.Current!.TryGetResource(key, ThemeVariant.Light, out var value)) return value!;
        throw new InvalidOperationException($"Missing design token {key}");
    }
    private static double Size(string key) => (double)Resource(key);
    private static SolidColorBrush Brush(string key) => new SolidColorBrush((Color)Resource(key));
    private double Gap => Size(model.DensityMode == DensityMode.Compact ? Tokens.TokenDensityCompactFormFieldGap : Tokens.TokenDensityComfortableFormFieldGap);
    private TextBlock Text(string text, bool heading = false)
    {
        var block = new TextBlock
        {
            Text = text, TextWrapping = TextWrapping.Wrap,
            FontFamily = (FontFamily)Resource(model.Urdu ? Tokens.TokenFontFamilyUrdu : Tokens.TokenFontFamilyLatin),
            FontSize = Size(heading ? Tokens.TokenFontSizeTitle : Tokens.TokenFontSizeBody),
            FontWeight = (FontWeight)Resource(heading ? Tokens.TokenFontWeightSemibold : Tokens.TokenFontWeightRegular),
            Foreground = Brush(Tokens.TokenColorTextPrimary)
        };
        return block;
    }
    private StackPanel Stack(params Control[] children)
    {
        var stack = new StackPanel { Spacing = Gap };
        foreach (var child in children) stack.Children.Add(child);
        return stack;
    }
    private StackPanel Row(params Control[] children)
    {
        var row = Stack(children); row.Orientation = Avalonia.Layout.Orientation.Horizontal; return row;
    }
    private Border Card(Control content) => new()
    {
        Child = content, Padding = new Thickness(Gap), Margin = new Thickness(Size(Tokens.TokenSpace1)),
        Background = Brush(Tokens.TokenColorSurfaceRaised), BorderBrush = Brush(Tokens.TokenColorBorderDefault),
        BorderThickness = new Thickness(Size(Tokens.TokenSpace1) / 4), CornerRadius = new CornerRadius(Size(Tokens.TokenRadiusLarge))
    };
    private static SpotButton Button(string label, string name, Func<Task> action, bool primary = false)
    {
        var button = new SpotButton { Name = name, Content = label, Variant = primary ? SpotButtonVariant.Primary : SpotButtonVariant.Subtle,
            Command = new RelayCommand(action) };
        AutomationProperties.SetName(button, label);
        return button;
    }
    private SpotButton CommandButton(string id, string name, CommandRoute route = CommandRoute.Tile, string? caption = null)
        => Button(caption ?? model.L(model.Registry[id].Label), name, () => InvokeAsync(id, route));
    public async Task<DispatchResult> InvokeAsync(string id, CommandRoute route, CommandContext? context = null)
    {
        var result = await model.RunAsync(id, route, context);
        if (!result.Executed && !result.Duplicate) ShowUnavailable(result);
        return result;
    }
    private Control? Focused() => this.GetVisualDescendants().OfType<Control>().FirstOrDefault(c => c.IsFocused);
    private static string? FocusName(Control? focused)
        => focused?.GetVisualAncestors().OfType<SpotTextInput>().FirstOrDefault()?.Name ?? focused?.Name ?? focused?.GetVisualAncestors().OfType<Control>().FirstOrDefault(c => c.Name is not null)?.Name;
    private void Restore(string? name, Control? fallback = null)
    {
        Dispatcher.UIThread.Post(() =>
        {
            var target = this.GetVisualDescendants().OfType<Control>().FirstOrDefault(c => c.Name == name && name is not null) ?? fallback;
            if (target is SpotTextInput input) input.TextBoxControl?.Focus();
            else if (target?.IsEffectivelyEnabled == true) target.Focus();
            else LauncherButton.Focus();
        });
    }
    private void Render()
    {
        var focus = Focused(); var focusName = FocusName(focus);
        if (layer.Length == 0 && focus is not null) { lastFocusName = focusName; lastFocusControl = focus; }
        if (renderedTab is { } previous && contentScroll is not null)
        {
            previous.ScrollOffset = contentScroll.Offset.Y;
            if (layer.Length == 0) previous.FocusName = focusName;
        }
        var switched = renderedTab != model.Tabs.Active;
        renderedTab = model.Tabs.Active;
        FlowDirection = model.Urdu ? FlowDirection.RightToLeft : FlowDirection.LeftToRight;
        Density.SetMode(this, model.DensityMode);
        var shell = new DockPanel { Background = Brush(Tokens.TokenColorSurfaceCanvas) };
        var top = Stack(BuildContext(), BuildMenus(), BuildTabs());
        if (model.Settings.Maintenance) top.Children.Add(Card(Text(model.L("Maintenance mode"), true)));
        top.Children.Add(Card(Text($"{model.L("Enforcement")}: {model.L(model.Settings.Enforcement.ToString())} · {model.L(model.Settings.Governance ? "Full Home" : "Simple Home")}")));
        DockPanel.SetDock(top, Dock.Top); shell.Children.Add(top);
        var status = Card(Text($"{model.L("Demo")} · {model.L("Mock connected")} · {model.L("Sync simulated")} · {model.L("Role")}: {model.L(MockBackend.RoleNames[(int)model.Home.User.Role])} · {model.StatusMessage}"));
        DockPanel.SetDock(status, Dock.Bottom); shell.Children.Add(status);
        var body = new Grid { ColumnDefinitions = new ColumnDefinitions("Auto,*,Auto") };
        if (model.Sidebar != SidebarMode.Hidden) body.Children.Add(BuildSidebar());
        contentScroll = new ScrollViewer { Content = BuildContent() };
        AutomationProperties.SetName(contentScroll, "Workspace content");
        Grid.SetColumn(contentScroll, 1); body.Children.Add(contentScroll);
        if (model.RailVisible) { var rail = BuildRail(); Grid.SetColumn(rail, 2); body.Children.Add(rail); }
        shell.Children.Add(body);
        root.Children.Remove(main); main = shell; root.Children.Insert(0, main);
        main.IsEnabled = layer.Length == 0;
        Dispatcher.UIThread.Post(() => contentScroll.Offset = new Vector(0, renderedTab?.ScrollOffset ?? 0));
        if (layer.Length == 0)
        {
            if (pendingRestoreName is { } pendingName)
            {
                // An overlay just closed and its answer (e.g. dirty Cancel) re-rendered
                // the shell; the origin control wins over the ordinary tab-focus restore.
                pendingRestoreName = null;
                Restore(pendingName, pendingRestoreFallback);
                pendingRestoreFallback = null;
            }
            else Restore(switched ? renderedTab?.FocusName : focusName);
        }
    }
    private Border BuildContext()
    {
        LauncherButton = CommandButton("shell.launcher", "LauncherButton", CommandRoute.Menu, model.L("Command launcher") + " · Ctrl+K");
        return Card(Row(Text("Spotledger", true), Text($"{model.L("Company")}: {model.Home.Company.Name}"),
            Text($"{model.L("Establishment")}: {model.L(model.Home.Company.Establishments[0].Name)}"), LauncherButton));
    }
    private WrapPanel BuildMenus()
    {
        var menus = new WrapPanel();
        foreach (var item in model.Registry.MenuItems(""))
        {
            var captured = item;
            menus.Children.Add(Button(model.L(item.Label), "Menu" + item.Path, () => ShowMenuAsync(captured.Path!, CommandRoute.Menu)));
        }
        return menus;
    }
    private DockPanel BuildTabs()
    {
        var tabs = new StackPanel { Orientation = Avalonia.Layout.Orientation.Horizontal, Spacing = Size(Tokens.TokenSpace1) };
        foreach (var tab in model.Tabs.Tabs)
        {
            var button = Button((tab.IsPinned ? "◆ " : "") + model.L(tab.Title) + (tab.IsDirty ? " *" : ""), "Tab" + tab.Key,
                () => { model.SelectTab(tab); return Task.CompletedTask; }, tab == model.Tabs.Active);
            var menu = new ContextMenu();
            foreach (var (label, id) in new[] { ("Close", "tab.close"), ("Close others", "tab.others"),
                ("Close to the right", "tab.right"), (tab.IsPinned ? "Unpin" : "Pin", "tab.pin") })
            {
                var item = new MenuItem { Header = model.L(label) };
                item.Click += async (_, _) => { model.SelectTab(tab); await InvokeAsync(id, CommandRoute.ContextMenu); };
                menu.Items.Add(item);
            }
            button.ContextMenu = menu;
            button.KeyDown += (_, e) =>
            {
                if (e.Key == Key.F10 && e.KeyModifiers == KeyModifiers.Shift) { menu.Open(button); e.Handled = true; }
            };
            tabs.Children.Add(button);
        }
        return new DockPanel
        {
            Children =
            {
                DockRight(CommandButton("tab.overflow", "AllTabs", CommandRoute.Menu)),
                new ScrollViewer { Content = tabs, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled }
            }
        };
    }
    private static Control DockRight(Control control) { DockPanel.SetDock(control, Dock.Right); return control; }
    private Border BuildSidebar()
    {
        var sidebar = Stack();
        foreach (var (key, label, icon) in new[] { ("home", "Home", "⌂"), ("tasks", "My Tasks", "✓"), ("processes", "Processes", "⇢"),
            ("sell", "Sell", "+"), ("buy", "Buy", "↓"), ("inventory", "Inventory", "▦"), ("reports", "Reports", "▤"), ("favorites", "Favorites", "★") })
        {
            var button = CommandButton("workspace." + key, "Sidebar" + key, caption: model.Sidebar == SidebarMode.Icons ? icon : icon + " " + model.L(label));
            AutomationProperties.SetName(button, model.L(label)); ToolTip.SetTip(button, model.L(label)); sidebar.Children.Add(button);
        }
        sidebar.Children.Add(CommandButton("view.sidebar", "SidebarToggle", caption: "☰"));
        return Card(sidebar);
    }
    private Border BuildContent()
    {
        NoteInput = null;
        if (model.Tabs.Active is not { } tab || tab.Key == "home") return BuildHome();
        var content = Stack(Text(model.L(tab.Title), true), Text(model.Boundary(tab)));
        if (tab.Record is { } record)
        {
            content.Children.Add(Text($"{model.L("Draft")} · UUID {record.Id} · v{record.Version}"));
            var noteInput = new SpotTextInput { Name = "RecordNote", Text = tab.Note, Placeholder = model.L("Note") };
            NoteInput = noteInput;
            AutomationProperties.SetName(noteInput, model.L("Note"));
            noteInput.PropertyChanged += (_, e) =>
            {
                if (e.Property != SpotTextInput.TextProperty) return;
                tab.Edit(noteInput.Text ?? "");
                // Update the marker without rebuilding the editor while typing.
                var button = this.GetVisualDescendants().OfType<SpotButton>().FirstOrDefault(b => b.Name == "Tab" + tab.Key);
                if (button is not null) button.Content = (tab.IsPinned ? "◆ " : "") + tab.Title + (tab.IsDirty ? " *" : "");
            };
            content.Children.Add(new SpotField { Label = model.L("Note"), Content = noteInput });
            content.Children.Add(Row(CommandButton("document.save", "SaveNote"), CommandButton("document.post", "PostDocument")));
        }
        else
        {
            var filter = new SpotTextInput { Name = "WorkspaceFilter", Text = tab.Filter, Placeholder = model.L("Filter") };
            filter.PropertyChanged += (_, e) => { if (e.Property == SpotTextInput.TextProperty) tab.Filter = filter.Text ?? ""; };
            content.Children.Add(new SpotField { Label = model.L("Filter"), Content = filter });
            content.Children.Add(CommandButton($"record.{MockBackend.RecordId}", "OpenSeedNote"));
        }
        return Card(content);
    }
    private Border BuildHome()
    {
        var home = Stack(Text($"{model.L("Greeting")}, {model.L(MockBackend.RoleNames[(int)model.Home.User.Role])}", true), Text(model.Date(model.Home.Now)));
        home.Children.Add(Text(model.L("Role shortcuts"), true));
        var shortcuts = new WrapPanel();
        foreach (var id in model.Home.Shortcuts) shortcuts.Children.Add(Card(CommandButton(id, "Shortcut" + id)));
        home.Children.Add(shortcuts);
        home.Children.Add(Text(model.L("My Tasks"), true));
        if (model.Home.Tasks.Count == 0)
            home.Children.Add(Card(Stack(Text(model.L("TaskEmpty")), CommandButton("workspace.reports", "EmptyReports"))));
        foreach (var task in model.Home.Tasks)
        {
            var due = task.Due < model.Home.Now ? "Overdue" : task.Due.Date == model.Home.Now.Date ? "Due today" : "Upcoming";
            home.Children.Add(Card(Stack(CommandButton(task.CommandId, "Task" + task.Title, caption: model.L(task.Title)),
                Text($"{model.L("Owner")}: {task.Owner} · {model.L(due)} · {model.Date(task.Due)}"))));
        }
        if (model.Settings.Governance)
        {
            home.Children.Add(Text(model.L("KPIs"), true));
            var kpis = new Grid { ColumnDefinitions = new ColumnDefinitions("*,*,*") };
            for (var i = 0; i < model.Home.Kpis.Count; i++)
            {
                var kpi = model.Home.Kpis[i];
                var card = Card(Stack(CommandButton(kpi.CommandId, "Kpi" + i, caption: model.L(kpi.Label)),
                    Text(kpi.Unit + " " + model.Number(kpi.Value), true), Text($"{model.L("Source")}: {kpi.Source}\n{model.L("As of")}: {model.Date(kpi.AsOf)}")));
                Grid.SetColumn(card, i); kpis.Children.Add(card);
            }
            home.Children.Add(kpis);
            home.Children.Add(Text(model.L("Process overview"), true));
            foreach (var process in model.Home.Processes)
                home.Children.Add(Card(Stack(CommandButton(process.CommandId, "ProcessOverview", caption: model.L(process.Name)),
                    Text($"{model.L("Active")}: {model.Number(process.Active)} · {model.L("Blocked")}: {model.Number(process.Blocked)}"))));
            home.Children.Add(Text(model.L("Recent activity"), true));
            foreach (var activity in model.Home.Activity)
                home.Children.Add(Card(Stack(CommandButton(activity.CommandId, "Activity" + activity.CommandId, caption: model.L(activity.Description)),
                    Text(activity.Owner + " · " + model.Date(activity.At)))));
        }
        return Card(home);
    }
    private Border BuildRail()
    {
        var rail = Stack(Text(model.L("Context"), true), CommandButton("view.rail", "RailToggle"), Text(model.L("My Tasks")));
        foreach (var task in model.Home.Tasks)
            rail.Children.Add(CommandButton(task.CommandId, "RailTask" + task.Title, caption: model.L(task.Title)));
        foreach (var record in model.Home.Records)
            rail.Children.Add(CommandButton("record." + record.Id, "RailRecord" + record.Id));
        var card = Card(rail); card.MaxWidth = Size(Tokens.TokenSpace12) * 5; return card;
    }
    private void BeginOverlay(string kind, Control content)
    {
        if (layer.Length == 0) { originControl = Focused() ?? lastFocusControl; originName = FocusName(originControl) ?? lastFocusName; }
        if (overlay is not null) root.Children.Remove(overlay);
        layer = kind; main.IsEnabled = false;
        overlay = Card(content);
        overlay.HorizontalAlignment = Avalonia.Layout.HorizontalAlignment.Center;
        overlay.VerticalAlignment = Avalonia.Layout.VerticalAlignment.Center;
        overlay.MaxWidth = Size(Tokens.TokenSpace12) * 14;
        overlay.MaxHeight = Math.Max(Size(Tokens.TokenSpace12), Bounds.Height - Size(Tokens.TokenSpace12));
        KeyboardNavigation.SetTabNavigation(overlay, KeyboardNavigationMode.Cycle);
        root.Children.Add(overlay);
    }
    public void CloseOverlay()
    {
        if (dirtyCompletion is { } pending)
        {
            dirtyCompletion = null; pending.TrySetResult(DirtyChoice.Cancel);
        }
        if (overlay is not null) root.Children.Remove(overlay);
        overlay = null; layer = ""; main.IsEnabled = true;
        // A close that answers the dirty prompt re-renders the tab area asynchronously;
        // queue the origin restore so it wins over whatever Render() then rebuilds.
        pendingRestoreName = originName; pendingRestoreFallback = originControl;
        Restore(originName, originControl);
    }
    private string? pendingRestoreName;
    private Control? pendingRestoreFallback;
    public async Task ShowMenuAsync(string path, CommandRoute route = CommandRoute.AltSequence)
    {
        menuPath = path; menuItems = model.Registry.MenuItems(path); selectedIndex = 0;
        await RenderMenuAsync(route);
    }
    private async Task RenderMenuAsync(CommandRoute route)
    {
        var content = Stack(Text(menuPath.Length == 0 ? model.L("File") : model.L(menuPath.Split('/')[^1]), true));
        for (var i = 0; i < menuItems.Count; i++)
        {
            var index = i; var item = menuItems[i];
            var reason = item.CommandId is null ? "" : (await model.Backend.GetAvailabilityAsync(item.CommandId, model.Context)).Value!.Reason;
            var label = $"[{item.Mnemonic}] {model.L(item.Label)}" + (item.Path is not null ? " →" : " · " + reason);
            content.Children.Add(Button(label, "MenuItem" + i, () => ActivateMenuAsync(index, route), i == selectedIndex));
        }
        content.Children.Add(Button(model.L("Close"), "MenuClose", () => { CloseOverlay(); return Task.CompletedTask; }));
        BeginOverlay("menu", new ScrollViewer { Content = content });
        Restore("MenuItem" + selectedIndex);
    }
    private async Task ActivateMenuAsync(int index, CommandRoute route)
    {
        var item = menuItems[index];
        if (item.Path is not null) { await ShowMenuAsync(item.Path, route); return; }
        CloseOverlay();
        if (item.CommandId is not null) await InvokeAsync(item.CommandId, route);
    }
    public sealed record LauncherEntry(CommandDefinition Definition, Availability Availability, string Label)
    {
        public override string ToString() => Label;
    }
    public void ShowLauncher()
    {
        var input = new SpotTextInput { Name = "LauncherSearch", Placeholder = model.L("Search commands") };
        AutomationProperties.SetName(input, model.L("Search commands"));
        launcherList = new ListBox { Name = "LauncherResults", FontSize = Size(Tokens.TokenFontSizeBody) };
        input.PropertyChanged += async (_, e) => { if (e.Property == SpotTextInput.TextProperty) await SearchAsync(input.Text ?? ""); };
        var content = Stack(Text(model.L("Command launcher"), true), input,
            new ScrollViewer { Content = launcherList, MaxHeight = Size(Tokens.TokenSpace12) * 7 },
            Row(Button(model.L("Open"), "RunLauncher", ExecuteLauncherAsync, true),
                Button(model.L("Close"), "CloseLauncher", () => { CloseOverlay(); return Task.CompletedTask; })));
        launcherList.DoubleTapped += async (_, _) => await ExecuteLauncherAsync();
        BeginOverlay("launcher", content);
        Restore("LauncherSearch");
        _ = SearchAsync("");
    }
    private int searchGeneration;
    public async Task SearchAsync(string query)
    {
        var generation = ++searchGeneration;
        var entries = new List<LauncherEntry>();
        foreach (var definition in model.Registry.Definitions.Where(d => string.Join(' ', d.SearchTerms.Prepend(d.Description).Prepend(d.Label).Prepend(model.L(d.Label)))
            .Contains(query, StringComparison.OrdinalIgnoreCase)).OrderBy(d => GroupOrder(d.Category)).ThenBy(d => d.Label, StringComparer.Ordinal))
        {
            var reply = await model.Backend.GetAvailabilityAsync(definition.Id, model.Context);
            var availability = reply.Value ?? new(AvailabilityState.Blocked, reply.Error!.Message);
            var chord = model.Keymap.Bindings.FirstOrDefault(b => b.CommandId == definition.Id)?.Chord;
            entries.Add(new(definition, availability, $"{model.L(definition.Category)} · {model.L(definition.Label)} {chord} · {availability.State}: {availability.Reason}"));
        }
        if (generation != searchGeneration || layer != "launcher") return;
        launcherEntries = entries; launcherList!.ItemsSource = entries; launcherList.SelectedIndex = entries.Count > 0 ? 0 : -1;
    }
    private static int GroupOrder(string category) => category switch { "Create" => 0, "Open" => 1, "Reports" => 2, _ => 3 };
    private async Task ExecuteLauncherAsync()
    {
        if (launcherList?.SelectedIndex is not { } index || index < 0 || index >= launcherEntries.Count) return;
        var entry = launcherEntries[index]; CloseOverlay(); await InvokeAsync(entry.Definition.Id, CommandRoute.Launcher);
    }
    private void ShowUnavailable(DispatchResult result)
    {
        var content = Stack(Text(model.L(result.Availability.State.ToString()), true), Text(result.Availability.Reason));
        if (result.Availability.ResolutionCommandId is { } id)
            content.Children.Add(Button(model.L("Resolve") + " · " + model.L(model.Registry[id].Label), "ResolveAvailability",
                async () => { CloseOverlay(); await InvokeAsync(id, CommandRoute.Launcher); }));
        content.Children.Add(Button(model.L("Close"), "CloseReason", () => { CloseOverlay(); return Task.CompletedTask; }));
        BeginOverlay("information", content); Restore("CloseReason");
    }
    private Task<DirtyChoice> ShowDirtyAsync(DemoTab tab)
    {
        dirtyCompletion = new(TaskCreationOptions.RunContinuationsAsynchronously);
        var completion = dirtyCompletion;
        var content = Stack(Text(model.L("Unsaved changes"), true), Text(tab.Title));
        foreach (var choice in Enum.GetValues<DirtyChoice>())
            content.Children.Add(Button(model.L(choice.ToString()), "Dirty" + choice,
                () => { dirtyCompletion = null; CloseOverlay(); completion.TrySetResult(choice); return Task.CompletedTask; }, choice == DirtyChoice.Save));
        BeginOverlay("dirty", content); Restore("DirtyCancel");
        return completion.Task;
    }
    private void ShowInformation(string message)
    {
        BeginOverlay("information", Stack(new ScrollViewer { Content = Text(message), MaxHeight = Size(Tokens.TokenSpace12) * 9 },
            Button(model.L("Close"), "CloseInformation", () => { CloseOverlay(); return Task.CompletedTask; })));
        Restore("CloseInformation");
    }
    private void ShowTabs()
    {
        var list = Stack(Text(model.L("All tabs"), true));
        foreach (var tab in model.Tabs.Tabs)
            list.Children.Add(Button(model.L(tab.Title) + (tab.IsDirty ? " *" : ""), "Overflow" + tab.Key,
                () => { CloseOverlay(); model.SelectTab(tab); return Task.CompletedTask; }));
        list.Children.Add(Button(model.L("Close"), "CloseTabs", () => { CloseOverlay(); return Task.CompletedTask; }));
        BeginOverlay("tabs", new ScrollViewer { Content = list });
        Restore(model.Tabs.Active is { } active ? "Overflow" + active.Key : "CloseTabs");
    }
    private async void HandleKeyDown(object? sender, KeyEventArgs e)
    {
        // Ctrl+Alt is AltGr on many layouts and must stay with the editor.
        if (e.KeyModifiers.HasFlag(KeyModifiers.Alt) && e.KeyModifiers.HasFlag(KeyModifiers.Control)) return;
        if (layer.Length > 0)
        {
            if (e.Key == Key.Escape)
            {
                e.Handled = true;
                if (layer == "menu" && menuPath.Length > 0)
                {
                    var parent = menuPath.Contains('/') ? menuPath[..menuPath.LastIndexOf('/')] : "";
                    await ShowMenuAsync(parent);
                }
                else CloseOverlay();
                return;
            }
            if (layer == "menu")
            {
                e.Handled = true;
                if (e.Key is Key.Down or Key.Up)
                {
                    selectedIndex = (selectedIndex + (e.Key == Key.Down ? 1 : -1) + menuItems.Count) % menuItems.Count;
                    await RenderMenuAsync(CommandRoute.AltSequence);
                }
                else if (e.Key is Key.Enter or Key.Right) await ActivateMenuAsync(selectedIndex, CommandRoute.AltSequence);
                else if (e.Key == Key.Left && menuPath.Length > 0)
                    await ShowMenuAsync(menuPath.Contains('/') ? menuPath[..menuPath.LastIndexOf('/')] : "");
                else
                {
                    var key = e.Key.ToString();
                    var index = key.Length == 1 ? menuItems.ToList().FindIndex(i => i.Mnemonic == key[0]) : -1;
                    if (index >= 0) await ActivateMenuAsync(index, CommandRoute.AltSequence);
                    else e.Handled = false;
                }
            }
            else if (layer == "launcher" && e.Key is Key.Down or Key.Up)
            {
                if (launcherEntries.Count > 0) launcherList!.SelectedIndex = (launcherList.SelectedIndex + (e.Key == Key.Down ? 1 : -1) + launcherEntries.Count) % launcherEntries.Count;
                e.Handled = true;
            }
            else if (layer == "launcher" && e.Key == Key.Enter) { e.Handled = true; await ExecuteLauncherAsync(); }
            // No global shortcuts while a modal interaction owns focus.
            return;
        }
        if (model.Keymap.AltMenus && e.Key is Key.LeftAlt or Key.RightAlt && e.KeyModifiers is KeyModifiers.None or KeyModifiers.Alt)
        { e.Handled = true; await ShowMenuAsync(""); return; }
        var modifiers = e.KeyModifiers;
        var chord = (modifiers.HasFlag(KeyModifiers.Control) ? "Ctrl+" : "") +
            (modifiers.HasFlag(KeyModifiers.Shift) ? "Shift+" : "") + (modifiers.HasFlag(KeyModifiers.Alt) ? "Alt+" : "") + e.Key;
        var binding = model.Keymap.Bindings.FirstOrDefault(b => b.Chord == chord);
        if (binding is not null) { e.Handled = true; await InvokeAsync(binding.CommandId, CommandRoute.Shortcut); }
    }
}
