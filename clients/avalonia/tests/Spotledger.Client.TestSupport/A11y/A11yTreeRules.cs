using Avalonia;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Animation;
using Avalonia.Controls;
using Avalonia.VisualTree;

namespace Spotledger.Client.TestSupport.A11y;

/// <summary>
/// The rules that read only the visual tree: naming, sibling ambiguity, grid exposure, tab order
/// and motion. Every rule returns findings instead of throwing, so one broken control never hides
/// the rest of the report.
/// </summary>
internal static class A11yTreeRules
{
    public static void Name(IReadOnlyList<Control> controls, string scope, A11yOptions options, List<A11yFinding> findings, Visual pathRoot)
    {
        foreach (var control in controls)
        {
            if (!A11yTree.IsVisibleAndEnabled(control) || !A11yTree.IsInteractive(control)) continue;
            if (A11yTree.IsIconOnly(control) && A11yTree.AccessibleName(control) is null)
            {
                // The more specific rule owns the finding; do not report the same defect twice.
                if (options.Runs(A11yRule.IconOnlyButton))
                    findings.Add(new(scope, A11yRule.IconOnlyButton, control.GetType().Name, A11yTree.Path(control, pathRoot),
                        "A button whose content is only a graphic has no accessible name; set AutomationProperties.Name."));
                continue;
            }
            if (options.Runs(A11yRule.MissingName) && A11yTree.AccessibleName(control) is null)
                findings.Add(new(scope, A11yRule.MissingName, control.GetType().Name, A11yTree.Path(control, pathRoot),
                    $"{A11yTree.Role(control)} has no accessible name and no label association (AutomationProperties.LabeledBy)."));
        }
    }

    public static void DuplicateSiblings(IReadOnlyList<Control> controls, string scope, A11yOptions options, List<A11yFinding> findings, Visual pathRoot, Visual treeRoot)
    {
        if (!options.Runs(A11yRule.DuplicateSibling)) return;
        foreach (var parent in controls.Prepend((Control)treeRoot))
        {
            var groups = A11yTree.Descendants(parent).Where(A11yTree.IsInteractive).Where(A11yTree.IsVisibleAndEnabled)
                .Where(control => ReferenceEquals(control.GetVisualParent(), parent))
                .Select(control => (Control: control, Name: A11yTree.AccessibleName(control), Role: A11yTree.Role(control)))
                .Where(item => !string.IsNullOrWhiteSpace(item.Name))
                .Select(item => (item.Control, item.Name, item.Role))
                .GroupBy(item => (item.Name, item.Role));
            foreach (var group in groups)
            {
                var items = group.ToList();
                if (items.Count < 2) continue;
                foreach (var item in items.Skip(1))
                    findings.Add(new(scope, A11yRule.DuplicateSibling, item.Control.GetType().Name, A11yTree.Path(item.Control, pathRoot),
                        $"{items.Count} siblings share the {item.Role} name \"{item.Name}\"; a screen reader cannot tell them apart."));
            }
        }
    }

    public static void GridPatterns(IReadOnlyList<Control> controls, string scope, A11yOptions options, List<A11yFinding> findings, Visual pathRoot)
    {
        if (!options.Runs(A11yRule.GridPatterns)) return;
        foreach (var grid in controls.Where(IsCustomDrawnGrid).Where(A11yTree.IsVisibleAndEnabled))
        {
            var path = A11yTree.Path(grid, pathRoot);
            var control = grid.GetType().Name;
            var peer = A11yTree.Peer(grid);
            var type = peer?.GetAutomationControlType() ?? AutomationControlType.None;
            if (peer is null || type == AutomationControlType.None)
            {
                findings.Add(new(scope, A11yRule.GridPatterns, control, path,
                    "Custom-drawn grid has no grid automation peer (role None); a screen reader sees an anonymous box."));
                continue;
            }
            if (type is not (AutomationControlType.DataGrid or AutomationControlType.Table or AutomationControlType.List or AutomationControlType.Tree))
            {
                findings.Add(new(scope, A11yRule.GridPatterns, control, path,
                    $"Grid peer reports {type}; a grid must report DataGrid, Table, List or Tree."));
                continue;
            }
            var children = peer.GetChildren();
            var rowItems = children.Count(child => child.GetAutomationControlType() is AutomationControlType.DataItem or AutomationControlType.ListItem or AutomationControlType.Custom);
            if (rowItems == 0)
                findings.Add(new(scope, A11yRule.GridPatterns, control, path,
                    "Grid peer exposes no row items; rows and cells cannot be reached one by one."));
            // Avalonia 12 has no IGridProvider/ITableProvider; row+selection patterns are what a
            // screen reader can actually use, so that is the contract this rule holds.
            var selection = peer.GetProvider<ISelectionProvider>() is not null
                || children.Any(child => child.GetProvider<ISelectionItemProvider>() is not null);
            if (!selection)
                findings.Add(new(scope, A11yRule.GridPatterns, control, path,
                    "Grid exposes no selection pattern, so the focused row cannot be reported."));
        }
    }

    /// <summary>A control that draws its own rows and cells (the Spot catalogue's grid), not a layout panel.</summary>
    internal static bool IsCustomDrawnGrid(Control control) =>
        control.GetType().Name.EndsWith("Grid", StringComparison.Ordinal)
        && control is not Avalonia.Controls.Grid
        && control is not Avalonia.Controls.Primitives.UniformGrid
        && control.GetType().Namespace?.StartsWith("Avalonia", StringComparison.Ordinal) != true;

    public static void TabOrder(IReadOnlyList<Control> controls, string scope, A11yOptions options, List<A11yFinding> findings, Visual pathRoot, Visual geometryRoot)
    {
        if (!options.Runs(A11yRule.TabOrder)) return;
        var focusable = controls.Where(control => control.Focusable && control.IsTabStop && A11yTree.IsVisibleAndEnabled(control)).ToList();
        if (focusable.Count < 2) return;
        var treeOrder = focusable.Select((control, index) => (Control: control, Index: index)).ToDictionary(item => item.Control, item => item.Index);
        var tabOrder = focusable.OrderBy(control => control.TabIndex).ThenBy(control => treeOrder[control]).ToList();
        var reading = A11yTree.ReadingOrder(focusable, geometryRoot);
        var readingPosition = reading.Select((control, index) => (control, index)).ToDictionary(item => item.control, item => item.index);
        for (var index = 1; index < tabOrder.Count; index++)
        {
            var previous = tabOrder[index - 1];
            var current = tabOrder[index];
            if (!readingPosition.TryGetValue(previous, out var previousRead) || !readingPosition.TryGetValue(current, out var currentRead)) continue;
            if (currentRead < previousRead)
                findings.Add(new(scope, A11yRule.TabOrder, current.GetType().Name, A11yTree.Path(current, pathRoot),
                    $"Tab order reaches {A11yTree.Describe(previous)} (tab #{index}) before {A11yTree.Describe(current)} (tab #{index + 1}), "
                    + $"but {A11yTree.Describe(current)} is drawn earlier (reading #{currentRead + 1} vs #{previousRead + 1})."));
        }
    }

    public static void Motion(IEnumerable<Visual> visuals, string scope, A11yOptions options, List<A11yFinding> findings, Visual pathRoot)
    {
        if (!options.Runs(A11yRule.Motion)) return;
        var bound = ReducedMotion.Bound;
        foreach (var visual in visuals)
        {
            if (visual is not Control control) continue;
            foreach (var transition in control.Transitions ?? [])
            {
                if (transition is not TransitionBase animation || animation.Duration <= bound) continue;
                findings.Add(new(scope, A11yRule.Motion, control.GetType().Name, A11yTree.Path(control, pathRoot),
                    $"Transition lasts {animation.Duration.TotalMilliseconds:0} ms; the reduced-motion bound is {bound.TotalMilliseconds:0} ms."));
            }
        }
    }

    /// <summary>The tree in visual order, including the root, for transition inspection.</summary>
    internal static IEnumerable<Visual> Visuals(Visual root) => root.GetSelfAndVisualDescendants();
}
