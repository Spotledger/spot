using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;

namespace Spotledger.Client.Shell.Views;

/// <summary>
/// Builds the native menu bar (<see cref="Menu"/> + <see cref="MenuItem"/>) from the
/// command registry. Desktop conventions come from Avalonia's own menu engine — click
/// opens, second click/Escape closes, hover-switches between top-levels, arrows/Home/End/
/// Enter navigate, Alt+letter opens a menu or fires an item, mnemonics underline while
/// Alt is held. This builder adds: access-key registration per item, the keymap's gesture
/// text in the shortcut column, grey-out with the dispatcher's reason as tooltip, and one
/// dispatch path — every item click executes its <see cref="ShellCommand"/> through the
/// <see cref="CommandDispatcher"/> like every other route.
/// </summary>
public static class MenuBarBuilder
{
    public static Menu Build(ShellViewModel shell)
    {
        var menu = new Menu { Name = "MenuBar" };
        Populate(menu, shell);
        return menu;
    }

    /// <summary>Fills an existing <see cref="Menu"/> from the shell's registry.</summary>
    public static void Populate(Menu menu, ShellViewModel shell)
    {
        AutomationProperties.SetName(menu, "Application menu");
        menu.Items.Clear();
        foreach (var top in shell.Registry.Branches.Where(branch => !branch.Path.Contains('/', StringComparison.Ordinal)))
        {
            menu.Items.Add(BuildBranchItem(shell, top));
        }
    }

    private static MenuItem BuildBranchItem(ShellViewModel shell, MenuBranch branch)
    {
        var item = new MenuItem
        {
            Header = Header(shell, branch.Label, branch.Mnemonic),
            Name = "Menu_" + branch.Path.Replace('/', '_'),
        };
        foreach (var child in shell.Registry.MenuItems(branch.Path))
        {
            if (child.Path is { } path)
            {
                var sub = shell.Registry.Branches.First(branch => branch.Path == path);
                item.Items.Add(BuildBranchItem(shell, sub));
            }
            else
            {
                item.Items.Add(BuildCommandItem(shell, shell.Commands[child.CommandId!]));
            }
        }

        return item;
    }

    private static MenuItem BuildCommandItem(ShellViewModel shell, ShellCommand command)
    {
        var item = new MenuItem
        {
            Header = Header(shell, command.Label, command.Definition.Mnemonic),
            Command = command,
            Name = "MenuItem_" + command.Id.Replace('.', '_'),
        };
        if (command.GestureText is { } chord)
        {
            item.InputGesture = ParseGesture(chord);
        }

        if (command.Reason is { } reason)
        {
            ToolTip.SetTip(item, reason);
        }

        // Enablement follows the command object (CanExecute + CanExecuteChanged on the
        // state event); the tooltip refreshes with the current reason.
        shell.CommandsStateChanged += (_, _) =>
        {
            ToolTip.SetTip(item, command.Reason);
        };
        return item;
    }

    /// <summary>The header text with the access-key underscore; the theme's AccessText
    /// underlines the mnemonic while Alt is held (native behaviour).</summary>
    private static string Header(ShellViewModel shell, string label, char mnemonic) =>
        mnemonic is not ' ' ? AccessKey(label, mnemonic) : label;

    /// <summary>Inserts the Avalonia access-key underscore before the mnemonic letter.</summary>
    internal static string AccessKey(string label, char mnemonic)
    {
        var index = label.IndexOf(mnemonic, StringComparison.OrdinalIgnoreCase);
        if (index < 0)
        {
            return label;
        }

        return string.Concat(label.AsSpan(0, index), "_", label.AsSpan(index));
    }

    /// <summary>Turns a keymap chord into the gesture shown in the menu's shortcut column.</summary>
    internal static KeyGesture ParseGesture(string chord)
    {
        var (key, modifiers) = KeymapProfile.ParseChord(chord);
        return new KeyGesture(key, modifiers);
    }
}
