using System.ComponentModel;
using System.Globalization;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Shell.Views;

/// <summary>
/// The signed-in application chrome (WP-UI-02.T2): one window frame — header row (logo,
/// native menu bar, Search (Ctrl+K), New button, bell with count, user chip), tab strip,
/// collapsible icon sidebar, content, the Ctrl+K command launcher as a floating panel and
/// a status bar. Every interactive element dispatches through the shell's single command
/// model. Kept from T1: the session header data, the FlowDirection switch, logout,
/// Ctrl+Tab and Ctrl+W (now routed through the keymap + dispatcher).
/// </summary>
public sealed partial class ShellView : UserControl
{
    private ShellViewModel? _shell;
    private Menu? _menu;
    private TextBox? _searchBox;
    private TextBox? _launcherInput;
    private StackPanel? _launcherResults;
    private Border? _launcherPanel;
    private Border? _dialogPanel;
    private StackPanel? _dialogButtons;
    private TextBlock? _dialogMessage;
    private ItemsControl? _tabStrip;
    private ItemsControl? _sidebarItems;
    private ContentControl? _tabContent;
    private Button? _overflowButton;
    private TextBlock? _statusText;
    private TextBlock? _profileText;
    private TextBlock? _densityText;
    private Control? _focusOrigin;
    private TaskCompletionSource<TabCloseChoice>? _closePrompt;
    private bool _launcherGuard;

    public ShellView()
    {
        InitializeComponent();
        var scope = NameScope.GetNameScope(this)!;
        _searchBox = scope.Get<TextBox>("SearchBox");
        _launcherInput = scope.Get<TextBox>("LauncherInput");
        _launcherResults = scope.Get<StackPanel>("LauncherResults");
        _launcherPanel = scope.Get<Border>("LauncherPanel");
        _dialogPanel = scope.Get<Border>("DialogPanel");
        _dialogButtons = scope.Get<StackPanel>("DialogButtons");
        _dialogMessage = scope.Get<TextBlock>("DialogMessage");
        _tabStrip = scope.Get<ItemsControl>("TabStrip");
        _sidebarItems = scope.Get<ItemsControl>("SidebarItems");
        _tabContent = scope.Get<ContentControl>("TabContent");
        _overflowButton = scope.Get<Button>("TabOverflowButton");
        _statusText = scope.Get<TextBlock>("StatusText");
        _profileText = scope.Get<TextBlock>("ProfileText");
        _densityText = scope.Get<TextBlock>("DensityText");
        var menuHost = scope.Get<ContentControl>("MenuBarHost");
        _menu = new Menu { Name = "MenuBar" };
        if (menuHost is not null)
        {
            menuHost.Content = _menu;
        }

        SetGeometry(scope, "LogoGlyph", "logo");
        SetGeometry(scope, "NewGlyph", "plus");
        SetGeometry(scope, "BellGlyph", "bell");
        SetGeometry(scope, "UserChevron", "chevron-down");
        SetGeometry(scope, "MoreGlyph", "more");
        SetGeometry(scope, "ToggleGlyph", "panel");
        SetGeometry(scope, "ConnectionGlyph", "connection");

        scope.Get<Button>("NewButton")!.Click += (_, _) => Fire("shell.new", CommandRoute.ContextMenu);
        scope.Get<Button>("SidebarToggle")!.Click += (_, _) => Fire("view.sidebar", CommandRoute.ContextMenu);
        scope.Get<Button>("SidebarRestoreButton")!.Click += (_, _) =>
        {
            if (_shell is { Sidebar: SidebarMode.Hidden } hiddenShell)
            {
                hiddenShell.Sidebar = SidebarMode.Expanded;
            }
            else
            {
                Fire("view.sidebar", CommandRoute.ContextMenu);
            }
        };
        SetGeometry(scope, "RestoreGlyph", "panel");
        scope.Get<Button>("NewTabButton")!.Click += (_, _) => Fire("tab.new", CommandRoute.ContextMenu);
        _overflowButton!.Click += (_, _) => OpenTabOverflow();
        _searchBox!.KeyDown += (_, e) =>
        {
            if (e.Key == Key.Enter && _shell is { IsLauncherOpen: false } shell)
            {
                shell.OpenLauncher(_searchBox.Text);
                _searchBox.Text = string.Empty;
                e.Handled = true;
            }
        };
        _launcherInput!.KeyDown += (_, e) =>
        {
            // Launcher arrow/Enter/Escape keys are handled in OnKeyDown while it is open.
            if (e.Key == Key.Enter && _shell is { IsLauncherOpen: true })
            {
                _ = RunLauncherSelectionAsync();
                e.Handled = true;
            }
        };
        _launcherInput!.TextChanged += (_, _) =>
        {
            if (!_launcherGuard && _shell is { IsLauncherOpen: true } shell)
            {
                shell.Launcher.Query = _launcherInput.Text ?? string.Empty;
            }
        };
    }

    private static void SetGeometry(INameScope scope, string name, string icon)
    {
        if (scope.Get<Avalonia.Controls.Shapes.Path>(name) is { } path)
        {
            path.Data = ShellIcons.Data(icon);
            path.Stretch = Stretch.Uniform;
        }
    }

    internal Menu? MenuBar => _menu;

    internal TextBox SearchBoxControl => _searchBox!;

    internal TextBox LauncherInputControl => _launcherInput!;

    internal StackPanel LauncherResultsPanel => _launcherResults!;

    internal Border LauncherPanelControl => _launcherPanel!;

    internal Border DialogPanelControl => _dialogPanel!;

    internal ItemsControl TabStripControl => _tabStrip!;

    internal ItemsControl SidebarItemsControl => _sidebarItems!;

    internal ContentControl TabContentControl => _tabContent!;

    internal Button NewTabButtonControl => NameScope.GetNameScope(this)!.Get<Button>("NewTabButton")!;

    protected override void OnDataContextChanged(EventArgs e)
    {
        base.OnDataContextChanged(e);
        var shell = DataContext as ShellViewModel;
        if (shell is null || ReferenceEquals(shell, _shell))
        {
            return;
        }

        _shell = shell;
        shell.Launcher.StateChanged += (_, _) => RebuildLauncherRows();
        shell.LauncherOpenChanged += (_, _) => OnLauncherOpenChanged();
        shell.ClosePromptRequested = async tab => await AskCloseTabAsync(tab);
        shell.MenuBarFocusRequested += (_, _) => FocusMenuBar();
        shell.ClipboardActionRequested += RunClipboardAction;
        shell.Tabs.CollectionChanged += (_, _) => RebuildTabs();
        shell.ReSignInOpenChanged += (_, _) => UpdateSessionOverlays();
        shell.PropertyChanged += OnShellPropertyChanged;
        RebuildMenuBar();
        RebuildSidebar();
        RebuildTabs();
        RebuildLauncherRows();
        UpdateHeader();
        UpdateStatusBar();
        UpdateSessionOverlays();
        if (shell.Sidebar != SidebarMode.Expanded)
        {
            ApplySidebarMode();
        }
    }

    private void OnShellPropertyChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(ShellViewModel.CompactDensity):
                Density.SetMode(this, _shell!.CompactDensity ? DensityMode.Compact : DensityMode.Comfortable);
                UpdateStatusBar();
                break;
            case nameof(ShellViewModel.Sidebar):
                ApplySidebarMode();
                RebuildSidebar();
                break;
            case nameof(ShellViewModel.SelectedTab):
                RebuildTabs();
                RebuildSidebar();
                break;
            case nameof(ShellViewModel.SelectedWorkspace):
                RebuildSidebar();
                break;
            case nameof(ShellViewModel.UserName)
                 or nameof(ShellViewModel.SiteName)
                 or nameof(ShellViewModel.NotificationCount):
                UpdateHeader();
                break;
            case nameof(ShellViewModel.StatusMessage) or nameof(ShellViewModel.Keymap):
                UpdateStatusBar();
                RebuildMenuBar();
                break;
        }
    }

    protected override void OnAttachedToVisualTree(VisualTreeAttachmentEventArgs e)
    {
        base.OnAttachedToVisualTree(e);
        // The code-built chrome bakes token brushes (tab chips, sidebar, launcher rows);
        // a light/dark switch must rebuild them or the old variant's colours linger.
        if (TopLevel.GetTopLevel(this) is { } topLevel && !ReferenceEquals(topLevel, _themeTopLevel))
        {
            if (_themeTopLevel is not null)
            {
                _themeTopLevel.ActualThemeVariantChanged -= OnActualThemeVariantChanged;
            }

            _themeTopLevel = topLevel;
            topLevel.ActualThemeVariantChanged += OnActualThemeVariantChanged;
        }
    }

    private TopLevel? _themeTopLevel;

    private void OnActualThemeVariantChanged(object? sender, EventArgs e)
    {
        if (_shell is null)
        {
            return;
        }

        RebuildMenuBar();
        RebuildTabs();
        RebuildSidebar();
        RebuildLauncherRows();
        UpdateHeader();
    }

    private void Fire(string id, CommandRoute route)
    {
        if (_shell is not { } shell)
        {
            return;
        }

        // RunAsync reports a refused run on the status bar itself.
        _ = shell.RunAsync(id, route);
    }

    // ---- menu bar ----

    private void RebuildMenuBar()
    {
        if (_menu is null || _shell is not { } shell)
        {
            return;
        }

        _menu.Items.Clear();
        MenuBarBuilder.Populate(_menu, shell);
    }

    private void FocusMenuBar()
    {
        if (_menu?.Items.OfType<MenuItem>().FirstOrDefault() is { } first)
        {
            first.Focus();
            first.IsSelected = true;
        }
        else
        {
            _menu?.Focus();
        }
    }

    // ---- header ----

    private void UpdateHeader()
    {
        if (_shell is not { } shell)
        {
            return;
        }

        var scope = NameScope.GetNameScope(this)!;
        if (scope.Get<TextBlock>("UserNameText") is { } name)
        {
            name.Text = shell.UserName;
        }

        if (scope.Get<TextBlock>("SiteText") is { } site)
        {
            site.Text = shell.SiteName;
        }

        if (scope.Get<TextBlock>("UserInitials") is { } initials)
        {
            initials.Text = MakeInitials(shell.UserName);
        }

        var badge = scope.Get<Border>("BellBadge");
        var count = scope.Get<TextBlock>("BellCount");
        if (badge is not null)
        {
            badge.IsVisible = shell.HasNotifications;
        }

        if (count is not null)
        {
            count.Text = shell.NotificationCount.ToString(CultureInfo.InvariantCulture);
        }
    }

    internal static string MakeInitials(string fullName)
    {
        var parts = fullName.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        return parts.Length switch
        {
            0 => string.Empty,
            1 => parts[0][..Math.Min(2, parts[0].Length)].ToUpperInvariant(),
            _ => string.Concat(char.ToUpperInvariant(parts[0][0]), char.ToUpperInvariant(parts[^1][0])),
        };
    }

    private void UpdateStatusBar()
    {
        if (_shell is not { } shell)
        {
            return;
        }

        if (_statusText is not null)
        {
            _statusText.Text = shell.StatusMessage;
        }

        if (_profileText is not null)
        {
            _profileText.Text = $"Keyboard: {shell.Keymap.Name}";
        }

        if (_densityText is not null)
        {
            _densityText.Text = shell.DensityLabel;
        }
    }

    // ---- tabs ----

    private void RebuildTabs()
    {
        if (_tabStrip is null || _shell is not { } shell)
        {
            return;
        }

        _tabStrip.Items.Clear();
        foreach (var tab in shell.Tabs)
        {
            _tabStrip.Items.Add(BuildTabChip(shell, tab));
        }

        UpdateTabContent();
    }

    private static Border BuildTabChip(ShellViewModel shell, WorkspaceTabViewModel tab)
    {
        var isActive = ReferenceEquals(shell.SelectedTab, tab);
        var grid = new Grid { ColumnDefinitions = new ColumnDefinitions("Auto,Auto,Auto") };
        // The pin uses the Shell icon geometry (Path), not a Segoe private-use character:
        // the Latin token font has no glyph for \uE141 and would render a tofu box.
        var pin = ShellIcons.Glyph("pin", 12);
        pin.Classes.Add("chromeGlyph");
        pin.VerticalAlignment = VerticalAlignment.Center;
        pin.Margin = new Thickness(0, 0, 6, 0);
        pin.IsVisible = tab.IsPinned;
        Grid.SetColumn(pin, 0);
        grid.Children.Add(pin);
        var title = new TextBlock
        {
            Text = tab.Title + (tab.IsDirty ? " ·" : string.Empty),
            VerticalAlignment = VerticalAlignment.Center,
            FontSize = TokenDouble(Tokens.TokenFontSizeCaption),
        };
        BindBrush(title, TextBlock.ForegroundProperty, isActive ? Tokens.TokenColorBrand700 : Tokens.TokenColorTextSecondary);
        Grid.SetColumn(title, 1);
        grid.Children.Add(title);
        var close = new Button
        {
            Content = "×",
            FontSize = TokenDouble(Tokens.TokenFontSizeBody),
            Classes = { "chromeButton" },
            Padding = new Thickness(6, 0),
        };
        AutomationProperties.SetName(close, "Close tab " + tab.Title);
        close.Click += (_, _) => _ = shell.CloseTabInteractiveAsync(tab);
        Grid.SetColumn(close, 2);
        grid.Children.Add(close);
        var chip = new Border
        {
            Classes = { "tabItem" },
            Name = "Tab_" + tab.Key,
            Margin = new Thickness(0, 0, 4, 0),
            Padding = new Thickness(10, 4, 2, 4),
            Child = grid,
        };
        if (isActive)
        {
            chip.Classes.Add("active");
        }

        chip.ContextFlyout = BuildTabFlyout(shell, tab);
        chip.PointerPressed += (_, _) => shell.SelectTab(tab);
        return chip;
    }

    private static MenuFlyout BuildTabFlyout(ShellViewModel shell, WorkspaceTabViewModel tab)
    {
        var flyout = new MenuFlyout();
        void Add(string label, string commandId)
        {
            var command = shell.Commands[commandId];
            var item = new MenuItem { Header = label, Command = command };
            if (command.Reason is { } reason)
            {
                ToolTip.SetTip(item, reason);
            }

            // The flyout items act on the right-clicked tab, not whichever tab is active.
            item.Click += (_, _) =>
            {
                if (!ReferenceEquals(shell.SelectedTab, tab))
                {
                    shell.SelectedTab = tab;
                }
            };
            flyout.Items.Add(item);
        }

        Add("Close", "tab.close");
        Add("Close Others", "tab.others");
        Add("Close All", "tab.all");
        flyout.Items.Add(new Separator());
        Add(tab.IsPinned ? "Unpin Tab" : "Pin Tab", "tab.pin");
        Add("Duplicate", "tab.duplicate");
        Add("Move to New Window", "tab.newWindow");
        return flyout;
    }

    private void OpenTabOverflow()
    {
        if (_shell is not { } shell || _overflowButton is null)
        {
            return;
        }

        var flyout = new MenuFlyout();
        foreach (var tab in shell.Tabs)
        {
            var item = new MenuItem { Header = tab.Title };
            item.Click += (_, _) => shell.SelectedTab = tab;
            flyout.Items.Add(item);
        }

        _overflowButton.Flyout = flyout;
        flyout.ShowAt(_overflowButton);
    }

    private void UpdateTabContent()
    {
        if (_tabContent is null || _shell is not { } shell)
        {
            return;
        }

        if (shell.SelectedTab is not { } tab)
        {
            _tabContent.Content = null;
            return;
        }

        var panel = new StackPanel
        {
            Spacing = 8,
            Margin = new Thickness(24),
            VerticalAlignment = VerticalAlignment.Top,
        };
        var heading = new TextBlock
        {
            Text = tab.Title,
            FontSize = TokenDouble(Tokens.TokenFontSizeHeading),
            FontWeight = FontWeight.SemiBold,
        };
        BindBrush(heading, TextBlock.ForegroundProperty, Tokens.TokenColorTextPrimary);
        panel.Children.Add(heading);
        var body = new TextBlock
        {
            // An unbuilt workspace says in one line what it will hold (brief §5).
            Text = tab.Description ?? WorkspaceDescriptions.For(tab.Workspace),
            FontSize = TokenDouble(Tokens.TokenFontSizeBody),
            TextWrapping = TextWrapping.Wrap,
        };
        BindBrush(body, TextBlock.ForegroundProperty, Tokens.TokenColorTextSecondary);
        panel.Children.Add(body);
        _tabContent.Content = panel;
    }

    // ---- sidebar ----

    private void RebuildSidebar()
    {
        if (_sidebarItems is null || _shell is not { } shell)
        {
            return;
        }

        _sidebarItems.Items.Clear();
        foreach (var (commandId, label) in ShellCommands.Workspaces)
        {
            _sidebarItems.Items.Add(BuildSidebarRow(shell, commandId, label));
        }

        ApplySidebarMode();
    }

    private Border BuildSidebarRow(ShellViewModel shell, string commandId, string label)
    {
        var selected = string.Equals(shell.SelectedWorkspace, label, StringComparison.Ordinal);
        var icon = ShellIcons.Glyph(SidebarIcon(commandId), 18);
        icon.Classes.Add("chromeGlyph");
        var text = new TextBlock
        {
            Text = label,
            VerticalAlignment = VerticalAlignment.Center,
            Margin = new Thickness(8, 0, 0, 0),
            FontSize = TokenDouble(Tokens.TokenFontSizeCaption),
        };
        text.Classes.Add("chromeLabel");
        var stack = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Name = "SidebarLabel_" + label,
        };
        stack.Children.Add(icon);
        stack.Children.Add(text);
        var row = new Border
        {
            Classes = { "sidebarRow" },
            Name = "Sidebar_" + label,
            Padding = new Thickness(8, 6),
            Child = stack,
            Focusable = true,
        };
        if (selected)
        {
            row.Classes.Add("selected");
            BindBrush(text, TextBlock.ForegroundProperty, Tokens.TokenColorBrand700);
            BindBrush(icon, Avalonia.Controls.Shapes.Path.StrokeProperty, Tokens.TokenColorBrand700);
        }

        AutomationProperties.SetName(row, label);
        row.PointerPressed += (_, _) => Fire(commandId, CommandRoute.ContextMenu);
        row.KeyDown += (_, e) =>
        {
            if (e.Key is Key.Enter or Key.Space)
            {
                Fire(commandId, CommandRoute.ContextMenu);
                e.Handled = true;
            }
        };
        return row;
    }

    private static string SidebarIcon(string commandId) => commandId switch
    {
        "workspace.home" => "home",
        "workspace.myTasks" => "tasks",
        "workspace.selling" => "selling",
        "workspace.buying" => "buying",
        "workspace.stock" => "stock",
        "workspace.accounts" => "accounts",
        "workspace.reports" => "reports",
        _ => "settings",
    };

    private void ApplySidebarMode()
    {
        if (_shell is not { } shell || _sidebarItems is null)
        {
            return;
        }

        var scope = NameScope.GetNameScope(this)!;
        var hidden = shell.Sidebar == SidebarMode.Hidden;
        scope.Get<Border>("SidebarBorder")!.IsVisible = !hidden;
        scope.Get<Button>("SidebarRestoreButton")!.IsVisible = hidden;
        foreach (var stack in _sidebarItems.Items.OfType<Border>().Select(row => row.Child).OfType<StackPanel>())
        {
            var label = stack.Children.OfType<TextBlock>().FirstOrDefault();
            if (label is not null)
            {
                label.IsVisible = shell.Sidebar == SidebarMode.Expanded;
            }
        }
    }

    // ---- launcher ----

    private void OnLauncherOpenChanged()
    {
        if (_shell is not { } shell)
        {
            return;
        }

        if (shell.IsLauncherOpen)
        {
            SaveFocusOrigin();
            _launcherPanel!.IsVisible = true;
            _launcherGuard = true;
            _launcherInput!.Text = shell.Launcher.Query;
            _launcherGuard = false;
            RebuildLauncherRows();
            _launcherInput.Focus();
        }
        else
        {
            _launcherPanel!.IsVisible = false;
            RestoreFocusOrigin();
        }
    }

    private void RebuildLauncherRows()
    {
        if (_launcherResults is null || _shell is not { } shell)
        {
            return;
        }

        _launcherResults.Children.Clear();
        var launcher = shell.Launcher;
        if (launcher.ShowingKeymap)
        {
            _launcherResults.Children.Add(LauncherGroupHeader($"Keyboard map — {launcher.KeymapName} profile"));
            foreach (var binding in launcher.KeymapBindings)
            {
                _launcherResults.Children.Add(LauncherRow(new LauncherEntry(
                    new CommandDefinition(binding.CommandId, binding.CommandId, binding.Chord, "Open", "", ' '),
                    "Open",
                    CommandAvailability.Available), binding.Chord));
            }

            return;
        }

        Border? selectedRow = null;
        foreach (var (title, entries) in launcher.Groups)
        {
            _launcherResults.Children.Add(LauncherGroupHeader(title));
            foreach (var entry in entries)
            {
                var row = LauncherRow(entry, entry == launcher.Selected ? "Enter" : null);
                if (entry == launcher.Selected)
                {
                    selectedRow = row;
                }

                _launcherResults.Children.Add(row);
            }
        }

        // The palette clips at MaxHeight: scroll the highlighted row into view so what
        // Enter runs is what the user can see (brief §6). The freshly built rows have no
        // measured extent until a layout pass; keep trying each pass until it applies.
        if (selectedRow is not null)
        {
            void OnLayoutUpdated(object? sender, EventArgs e)
            {
                if (ScrollSelectedLauncherRow())
                {
                    selectedRow.LayoutUpdated -= OnLayoutUpdated;
                }
            }

            selectedRow.LayoutUpdated += OnLayoutUpdated;
            if (ScrollSelectedLauncherRow())
            {
                selectedRow.LayoutUpdated -= OnLayoutUpdated;
            }
        }
    }

    private bool ScrollSelectedLauncherRow()
    {
        if (_launcherResults is null || _launcherPanel is null
            || _launcherResults.Children.OfType<Border>().FirstOrDefault(row => row.Classes.Contains("launcherRowSelected")) is not { } row
            || NameScope.GetNameScope(this)!.Get<ScrollViewer>("LauncherScroll") is not { } scroller
            || row.Bounds.Height <= 0)
        {
            return false;
        }

        var rowTop = row.TranslatePoint(new Point(0, 0), _launcherResults)!.Value.Y;
        var max = Math.Max(0.0, scroller.Extent.Height - scroller.Viewport.Height);
        if (max <= 0)
        {
            return false;
        }

        // Keep the row fully inside the viewport: top-align when it hangs below, bottom-align when above.
        var target = rowTop + row.Bounds.Height > scroller.Offset.Y + scroller.Viewport.Height
            ? Math.Min(max, Math.Max(0.0, rowTop - 4))
            : Math.Max(0.0, rowTop + row.Bounds.Height - scroller.Viewport.Height + 4);
        scroller.Offset = new global::Avalonia.Vector(0, target);
        return true;
    }

    private static TextBlock LauncherGroupHeader(string title)
    {
        var header = new TextBlock
        {
            Text = title,
            FontWeight = FontWeight.SemiBold,
            FontSize = TokenDouble(Tokens.TokenFontSizeCaption),
            Margin = new Thickness(12, 8, 12, 4),
        };
        BindBrush(header, TextBlock.ForegroundProperty, Tokens.TokenColorTextSecondary);
        return header;
    }

    private Border LauncherRow(LauncherEntry entry, string? trailing)
    {
        var selected = trailing is not null;
        var grid = new Grid { ColumnDefinitions = new ColumnDefinitions("Auto,*,Auto,Auto") };
        var icon = ShellIcons.Glyph(LauncherIcon(entry), 16);
        BindBrush(icon, Avalonia.Controls.Shapes.Path.StrokeProperty, entry.Availability.IsAvailable
            ? Tokens.TokenColorTextPrimary
            : Tokens.TokenColorTextDisabled);
        Grid.SetColumn(icon, 0);
        grid.Children.Add(icon);
        var text = new StackPanel { Spacing = 1, Margin = new Thickness(10, 0, 0, 0) };
        var label = new TextBlock
        {
            Text = entry.Label,
            FontSize = TokenDouble(Tokens.TokenFontSizeBody),
        };
        BindBrush(label, TextBlock.ForegroundProperty, entry.Availability.IsAvailable
            ? Tokens.TokenColorTextPrimary
            : Tokens.TokenColorTextDisabled);
        text.Children.Add(label);
        var caption = new TextBlock
        {
            // A blocked command shows its reason instead of the description (brief §6).
            Text = entry.Caption,
            FontSize = TokenDouble(Tokens.TokenFontSizeCaption),
            TextTrimming = TextTrimming.CharacterEllipsis,
        };
        BindBrush(caption, TextBlock.ForegroundProperty, Tokens.TokenColorTextSecondary);
        text.Children.Add(caption);
        Grid.SetColumn(text, 1);
        grid.Children.Add(text);
        var chip = new Border
        {
            Classes = { "categoryChip" },
            Child = new TextBlock { Text = entry.CategoryChip, FontSize = TokenDouble(Tokens.TokenFontSizeCaption) },
        };
        Grid.SetColumn(chip, 2);
        grid.Children.Add(chip);
        if (trailing is not null)
        {
            var badge = new Border
            {
                Classes = { "keyBadge" },
                Margin = new Thickness(8, 0, 0, 0),
                Child = new TextBlock { Text = trailing, FontSize = TokenDouble(Tokens.TokenFontSizeCaption) },
            };
            Grid.SetColumn(badge, 3);
            grid.Children.Add(badge);
        }

        var row = new Border
        {
            Classes = { selected ? "launcherRowSelected" : "launcherRow" },
            Name = "LauncherRow_" + entry.Id,
            Padding = new Thickness(12, 6),
            Child = grid,
        };
        row.PointerPressed += (_, _) =>
        {
            if (_shell is null)
            {
                return;
            }

            var index = _shell.Launcher.Items.ToList().FindIndex(item => item.Id == entry.Id);
            if (index >= 0)
            {
                _shell.Launcher.SelectIndex(index);
                _ = RunLauncherSelectionAsync();
            }
        };
        return row;
    }

    private static string LauncherIcon(LauncherEntry entry) => entry.CategoryChip switch
    {
        "Create" => "plus",
        "Reports" => "reports",
        "Recent" => "more",
        _ => "chevron-right",
    };

    private async Task RunLauncherSelectionAsync()
    {
        if (_shell is null)
        {
            return;
        }

        await _shell.Launcher.RunSelectedAsync(CommandRoute.Launcher);
        _shell.CloseLauncher();
    }

    // ---- focus return ----

    private void SaveFocusOrigin() =>
        _focusOrigin = TopLevel.GetTopLevel(this)?.FocusManager?.GetFocusedElement() as Control;

    private void RestoreFocusOrigin()
    {
        if (_focusOrigin is { IsEffectivelyVisible: true } origin)
        {
            origin.Focus();
        }
        else
        {
            _searchBox?.Focus();
        }

        _focusOrigin = null;
    }

    // ---- dirty-close dialog ----

    private async Task<TabCloseChoice> AskCloseTabAsync(WorkspaceTabViewModel tab)
    {
        var prompt = new TaskCompletionSource<TabCloseChoice>(TaskCreationOptions.RunContinuationsAsynchronously);
        _closePrompt = prompt;
        if (_dialogMessage is not null)
        {
            _dialogMessage.Text = $"“{tab.Title}” has unsaved changes. Save before closing?";
        }

        _dialogButtons!.Children.Clear();
        _dialogButtons.Children.Add(DialogButton("Save", TabCloseChoice.Save, "accent"));
        _dialogButtons.Children.Add(DialogButton("Discard", TabCloseChoice.Discard, "flat"));
        _dialogButtons.Children.Add(DialogButton("Cancel", TabCloseChoice.Cancel, "flat"));
        SaveFocusOrigin();
        _dialogPanel!.IsVisible = true;
        _dialogButtons.Children.OfType<Button>().First().Focus();
        return await prompt.Task;
    }

    private Button DialogButton(string label, TabCloseChoice choice, string variantClass)
    {
        var button = new Button
        {
            Content = label,
            Name = "Dialog_" + label,
        };
        button.Classes.Add(variantClass);
        button.Click += (_, _) => AnswerClosePrompt(choice);
        return button;
    }

    private void AnswerClosePrompt(TabCloseChoice choice)
    {
        _dialogPanel!.IsVisible = false;
        var pending = _closePrompt;
        _closePrompt = null;
        pending?.TrySetResult(choice);
        RestoreFocusOrigin();
    }

    // ---- keyboard ----

    protected override void OnKeyDown(KeyEventArgs e)
    {
        if (_shell is { } shell)
        {
            if (shell.IsLauncherOpen)
            {
                switch (e.Key)
                {
                    case Key.Escape:
                        shell.CloseLauncher();
                        e.Handled = true;
                        break;
                    case Key.Up:
                        shell.Launcher.MoveSelection(-1);
                        e.Handled = true;
                        break;
                    case Key.Down:
                        shell.Launcher.MoveSelection(+1);
                        e.Handled = true;
                        break;
                    case Key.Enter:
                        _ = RunLauncherSelectionAsync();
                        e.Handled = true;
                        break;
                }

                if (e.Handled)
                {
                    base.OnKeyDown(e);
                    return;
                }
            }

            foreach (var binding in shell.Keymap.Bindings)
            {
                if (KeymapProfile.Matches(binding.Chord, e.Key, e.KeyModifiers))
                {
                    Fire(binding.CommandId, CommandRoute.Shortcut);
                    e.Handled = true;
                    break;
                }
            }
        }

        base.OnKeyDown(e);
    }

    private void RunClipboardAction(string action)
    {
        if (TopLevel.GetTopLevel(this)?.FocusManager?.GetFocusedElement() is not TextBox box)
        {
            return;
        }

        switch (action)
        {
            case "cut":
                box.Cut();
                break;
            case "copy":
                box.Copy();
                break;
            case "paste":
                box.Paste();
                break;
            case "selectAll":
                box.SelectAll();
                break;
        }
    }

    // ---- session overlays (WP-UI-02.T3): re-sign-in modal, password, sessions ----

    private void UpdateSessionOverlays()
    {
        if (_shell is not { } shell)
        {
            return;
        }

        var scope = NameScope.GetNameScope(this);
        if (scope is null)
        {
            return;
        }

        scope.Get<Border>("ReSignInPanel")!.IsVisible = shell.ShowReSignIn;
        var changePanel = scope.Get<Border>("ChangePasswordPanel");
        changePanel!.IsVisible = shell.ChangePasswordVisible;
        if (changePanel.IsVisible && changePanel.DataContext is not ChangePasswordViewModel)
        {
            changePanel.DataContext = shell.ChangePassword;
        }

        var sessionsPanel = scope.Get<Border>("MySessionsPanel");
        sessionsPanel!.IsVisible = shell.MySessionsVisible;
        if (sessionsPanel.IsVisible && sessionsPanel.DataContext is not MySessionsViewModel)
        {
            sessionsPanel.DataContext = shell.MySessions;
            scope.Get<ItemsControl>("SessionsList")!.ItemsSource = shell.MySessions?.Sessions;
        }

        if (shell.ShowReSignIn)
        {
            SaveFocusOrigin();
            scope.Get<TextBox>("ReSignInPasswordBox")!.Focus();
        }
    }

    // ---- token helpers ----

    /// <summary>
    /// Binds a brush property to a colour token so it follows the actual theme variant for as
    /// long as the element lives (WP-UI-15.T9). The chrome builds its tab chips, the tab-content
    /// heading/body, the sidebar labels and the launcher rows in code, each as a fresh control;
    /// the old <c>TokenBrush(key)</c> helper read <c>Application.Current.ActualThemeVariant</c>
    /// once and baked the result into a plain <see cref="SolidColorBrush"/>. Those controls are
    /// built from <see cref="OnDataContextChanged"/> — before the host window's theme variant is
    /// applied — so the bake-once brush froze on whatever variant was ambient at construction
    /// (typically Light), and the explicit value then overrode the reactive
    /// <c>DynamicResource</c> set by the <c>chromeLabel</c>/<c>chromeGlyph</c> styles. Over a Dark
    /// canvas that is dark text on a dark surface: the baseline bug this task fixes.
    /// <see cref="StyledElement.GetResourceObservable(object, Func{object?, object?}?)"/> is the
    /// code equivalent of a XAML <c>DynamicResource</c> binding — it pushes a new value whenever
    /// the resource or the actual theme variant changes, which a one-time lookup cannot.
    /// </summary>
    private static void BindBrush(StyledElement element, AvaloniaProperty property, string key) =>
        element.Bind(property, element.GetResourceObservable(key, value => value is Color color ? new SolidColorBrush(color) : value));

    private static double TokenDouble(string key) => TokenResolver.GetDouble(key);
}
