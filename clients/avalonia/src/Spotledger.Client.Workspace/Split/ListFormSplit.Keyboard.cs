using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.Controls;
using Spotledger.Client.Forms;
using Spotledger.Client.Lists;
using Spotledger.Client.Shell;
using Spotledger.Client.Workspace.Keyboard;
using AvaloniaGrid = Avalonia.Controls.Grid;

namespace Spotledger.Client.Workspace.Split;

/// <summary>The keyboard half of the arrangement: the chords, the commands they run and the guard's buttons.</summary>
public sealed partial class ListFormSplit
{
    private void Announce(string text) => _announcer.Say(text, Trace);

    /// <summary>True when the focused control is a text editor, which owns Ctrl+Z and Ctrl+Y.</summary>
    private bool IsTextEditorFocused()
        => TopLevel.GetTopLevel(this)?.FocusManager?.GetFocusedElement() is Visual focused
           && (focused is TextBox || Avalonia.VisualTree.VisualExtensions.FindAncestorOfType<TextBox>(focused, includeSelf: true) is not null);

    private void OnKey(object? sender, KeyEventArgs e)
    {
        if (_disposed)
        {
            return;
        }

        // Esc first: the guard and the form's own conflict panel own it while they are open.
        if (e.Key == Key.Escape && e.KeyModifiers == KeyModifiers.None)
        {
            if (IsGuardShown)
            {
                e.Handled = true;
                _ = AnswerGuardAsync(GuardChoice.Cancel);
                return;
            }

            if (_formContent.Content is DocumentFormView { Model: { Conflict: not null } })
            {
                return;
            }

            if (Panes.FocusedPane == FormPaneId)
            {
                e.Handled = true;
                _ = SelectRow(_currentId);
                FocusPane(ListPaneId, FocusRule.AfterCancel);
                return;
            }
        }

        // Arrow keys on the focused splitter resize the panes.
        if (ReferenceEquals(TopLevel.GetTopLevel(this)?.FocusManager?.GetFocusedElement(), _splitter)
            && e.Key is Key.Left or Key.Right
            && (e.KeyModifiers == KeyModifiers.None || e.KeyModifiers == KeyModifiers.Shift))
        {
            e.Handled = true;
            Nudge((e.Key == Key.Right ? 1 : -1) * (e.KeyModifiers == KeyModifiers.Shift ? 10 : 2));
            return;
        }

        // Enter or F2 in the list walks into the form instead of opening a tab, when there is a form pane.
        if ((e.Key == Key.Enter && e.KeyModifiers == KeyModifiers.None || e.Key == Key.F2)
            && _mode != SplitMode.List
            && Panes.FocusedPane == ListPaneId)
        {
            e.Handled = true;
            RequestFormFocus();
            return;
        }

        // Ctrl+Z, Ctrl+Y and Ctrl+Shift+Z outside a text editor are the form's own history (the Edit
        // menu's command, reached by key); inside an editor the field keeps them, which is why they
        // are not in the keymap. The decision is the shared one, so a bare Z or Y never undoes.
        switch (UndoShortcuts.Decide(e.Key, e.KeyModifiers, IsTextEditorFocused()))
        {
            case UndoShortcut.Undo:
                e.Handled = true;
                _ = Undo();
                return;
            case UndoShortcut.Redo:
                e.Handled = true;
                _ = Redo();
                return;
            default:
                break;
        }

        if (WorkspaceKeymaps.CommandOf(_keymap, e.Key, e.KeyModifiers) is not { } command)
        {
            return;
        }

        if (command == "tab.next" || command == "tab.previous")
        {
            e.Handled = true;
            TabCycleRequested?.Invoke(this, command == "tab.next" ? 1 : -1);
            return;
        }

        if (!SplitCommands.Ids.Contains(command, StringComparer.Ordinal))
        {
            return;
        }

        e.Handled = true;
        Execute(command);
    }

    private void Execute(string command)
    {
        switch (command)
        {
            case SplitCommands.ModeList:
                SetMode(SplitMode.List);
                break;
            case SplitCommands.ModeSplit:
                SetMode(SplitMode.Split);
                break;
            case SplitCommands.ModeForm:
                SetMode(SplitMode.Form);
                break;
            case SplitCommands.WalkNext:
                _ = WalkAsync(1);
                break;
            case SplitCommands.WalkPrevious:
                _ = WalkAsync(-1);
                break;
            case SplitCommands.FocusList:
                _ = FocusListPane();
                break;
            case SplitCommands.FocusForm:
                RequestFormFocus();
                break;
            case SplitCommands.OpenInTab:
                OpenInTab();
                break;
            case SplitCommands.NewFromList:
                _ = ShowAsync(null);
                break;
            case SplitCommands.SaveAndNext:
                LastOperation = SaveAndNextAsync();
                break;
            case SplitCommands.PaneNext:
                _ = CyclePanes(1);
                break;
            case SplitCommands.PanePrevious:
                _ = CyclePanes(-1);
                break;
            default:
                break;
        }
    }

    private async Task SaveAndNextAsync()
    {
        if (await SaveCoreAsync().ConfigureAwait(true))
        {
            await WalkCoreAsync(1).ConfigureAwait(true);
        }
    }

    private void OpenInTab()
    {
        if (_currentId is null && Form?.Model.DocumentId is null)
        {
            Announce("Save the new document before moving it to a tab");
            return;
        }

        var id = _currentId ?? Form?.Model.DocumentId;
        if (_documents.MoveToTab(id, moved =>
            {
                if (moved is not null)
                {
                    _list.Model.Open(moved);
                }
            }))
        {
            _formContent.Content = null;
            Announce("Opened in its own tab");
            OpenedInTab?.Invoke(this, id ?? string.Empty);
            _ = FocusListPane();
        }
    }

    private Border BuildGuard()
    {
        var bar = new Border { IsVisible = false, Padding = new Thickness(8, 6), Margin = new Thickness(0, 0, 0, 6) };
        AutomationProperties.SetAutomationId(bar, GuardId);
        AutomationProperties.SetName(bar, "Unsaved changes");

        var text = new TextBlock { TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
        AutomationProperties.SetAutomationId(text, "split.guard.question");

        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        AddGuardButton(buttons, "split.guard.save", "Save", SpotButtonVariant.Primary, GuardChoice.Save);
        AddGuardButton(buttons, "split.guard.discard", "Discard", SpotButtonVariant.Danger, GuardChoice.Discard);
        AddGuardButton(buttons, "split.guard.cancel", "Stay here", SpotButtonVariant.Secondary, GuardChoice.Cancel);

        var panel = new DockPanel { LastChildFill = true };
        DockPanel.SetDock(buttons, Dock.Right);
        panel.Children.Add(buttons);
        panel.Children.Add(text);
        bar.Child = panel;
        GuardQuestion = text;
        return bar;
    }

    private void AddGuardButton(Panel parent, string id, string label, SpotButtonVariant variant, GuardChoice choice)
    {
        var button = new SpotButton { Content = label, Variant = variant };
        AutomationProperties.SetAutomationId(button, id);
        AutomationProperties.SetName(button, label);
        button.Click += (_, _) => AnswerGuardRequested?.Invoke(this, choice);
        parent.Children.Add(button);
    }

    /// <summary>The guard's question, for a test that reads the text a screen reader would read.</summary>
    public TextBlock GuardQuestion { get; private set; } = new();

    /// <summary>Raised when a pointer clicked one of the guard's buttons (the keyboard path calls
    /// <see cref="AnswerGuardAsync"/> directly). A workflow never uses it: the guard is answered by key.</summary>
    public event EventHandler<GuardChoice>? AnswerGuardRequested;
}
