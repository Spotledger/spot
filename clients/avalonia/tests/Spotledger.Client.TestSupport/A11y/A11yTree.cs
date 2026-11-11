using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Controls.Primitives;
using Avalonia.VisualTree;

namespace Spotledger.Client.TestSupport.A11y;

/// <summary>
/// Read-only view of a visual tree for the audit: stable paths, accessible names, roles and
/// bounds in window coordinates. Nothing here mutates the tree or reads the clock.
/// </summary>
internal static class A11yTree
{
    /// <summary>Every control below <paramref name="root"/> in visual-tree order (repeats skip).</summary>
    public static IReadOnlyList<Control> Descendants(Control root)
    {
        ArgumentNullException.ThrowIfNull(root);
        var seen = new HashSet<Control>(ReferenceEqualityComparer.Instance);
        var result = new List<Control>();
        foreach (var visual in root.GetVisualDescendants())
            if (visual is Control control && seen.Add(control))
                result.Add(control);
        return result;
    }

    /// <summary>
    /// A stable path from <paramref name="root"/>: the automation id when a control declares one,
    /// otherwise `Type[index]` where index counts same-type siblings. Independent of run order.
    /// </summary>
    public static string Path(Visual control, Visual root)
    {
        ArgumentNullException.ThrowIfNull(control);
        ArgumentNullException.ThrowIfNull(root);
        var segments = new List<string>();
        for (var current = control; current is not null; current = current.GetVisualParent())
        {
            var automationId = current is Control typed ? AutomationProperties.GetAutomationId(typed) : null;
            segments.Add(string.IsNullOrWhiteSpace(automationId) ? Segment(current) : "#" + automationId);
            if (ReferenceEquals(current, root)) break;
        }
        segments.Reverse();
        return "/" + string.Join("/", segments);
    }

    private static string Segment(Visual visual)
    {
        var parent = visual.GetVisualParent();
        var index = 0;
        if (parent is not null)
            foreach (var sibling in parent.GetVisualChildren())
            {
                if (ReferenceEquals(sibling, visual)) break;
                if (sibling.GetType() == visual.GetType()) index++;
            }
        return $"{visual.GetType().Name}[{index}]";
    }

    /// <summary>
    /// The accessible name: an explicit <c>AutomationProperties.Name</c>, else a label linked with
    /// <c>LabeledBy</c>, else whatever the control's automation peer derives from its content.
    /// </summary>
    internal static string? AccessibleName(Control control)
    {
        ArgumentNullException.ThrowIfNull(control);
        var explicitName = AutomationProperties.GetName(control);
        if (!string.IsNullOrWhiteSpace(explicitName)) return explicitName.Trim();
        if (AutomationProperties.GetLabeledBy(control) is { } labeled)
        {
            var label = labeled switch
            {
                TextBlock text => text.Text,
                ContentControl content => content.Content as string,
                Control other => AutomationProperties.GetName(other),
                _ => null,
            };
            if (!string.IsNullOrWhiteSpace(label)) return label.Trim();
        }
        // What a screen reader reads from the control's own text, before asking the peer.
        var fromContent = control switch
        {
            TextBlock text => text.Text,
            ContentControl { Content: string content } => content,
            HeaderedContentControl { Header: string header } => header,
            _ => null,
        };
        if (!string.IsNullOrWhiteSpace(fromContent)) return fromContent.Trim();
        var peer = ControlAutomationPeer.CreatePeerForElement(control);
        var derived = peer?.GetName();
        if (string.IsNullOrWhiteSpace(derived) || IsTypeName(derived.Trim(), control)) return null;
        return derived.Trim();
    }

    /// <summary>
    /// A peer that reports its content's CLR type name ("Avalonia.Controls.Border") has not given the
    /// user a name: an icon-only button would otherwise pass every check.
    /// </summary>
    private static bool IsTypeName(string name, Control control) =>
        name == control.GetType().Name
        || name == control.GetType().FullName
        || name.StartsWith("Avalonia.", StringComparison.Ordinal)
        || name.StartsWith("Spotledger.", StringComparison.Ordinal)
        || Descendants(control).Any(descendant =>
            descendant.GetType().Name == name || descendant.GetType().FullName == name);

    /// <summary>The automation role, falling back to the CLR type when no peer exists.</summary>
    internal static string Role(Control control)
    {
        ArgumentNullException.ThrowIfNull(control);
        var peer = ControlAutomationPeer.CreatePeerForElement(control);
        return peer?.GetAutomationControlType().ToString() ?? control.GetType().Name;
    }

    internal static AutomationPeer? Peer(Control control) => ControlAutomationPeer.CreatePeerForElement(control);

    /// <summary>Bounds in <paramref name="root"/> coordinates, or null when not laid out.</summary>
    internal static Rect? BoundsIn(Visual control, Visual root)
    {
        ArgumentNullException.ThrowIfNull(control);
        ArgumentNullException.ThrowIfNull(root);
        if (control.Bounds.Width <= 0 || control.Bounds.Height <= 0) return null;
        var origin = control.TranslatePoint(new Point(0, 0), root);
        return origin is { } point ? new Rect(point, control.Bounds.Size) : null;
    }

    internal static bool IsVisibleAndEnabled(Control control) =>
        control.IsEffectivelyVisible && control.IsEffectivelyEnabled && control.Bounds.Width > 0 && control.Bounds.Height > 0;

    /// <summary>
    /// The interactive controls a screen-reader or keyboard user must be able to name: the brief's
    /// button/text box/combo box/check box/list item/tab item/menu item list, plus the Spot
    /// catalogue's input controls (they derive from these) and the grid cell editors when real.
    /// </summary>
    internal static bool IsInteractive(Control control) => control switch
    {
        Button => true,

        TextBox => true,
        ComboBox => true,
        ListBoxItem => true,
        TabItem => true,
        MenuItem => true,
        Slider => true,
        NumericUpDown => true,
        TreeViewItem => true,
        _ => false,
    };

    /// <summary>A button that shows only a picture: a graphic content and no text inside it.</summary>
    internal static bool IsIconOnly(Control control)
    {
        if (control is not Button button) return false;
        var content = button.Content;
        return content is not null and not string
            && content is Control { } element
            && element is not TextBlock
            && Descendants(element).All(descendant => descendant is not TextBlock { Text.Length: > 0 });
    }

    /// <summary>
    /// The four focusable controls' reading order: rows top-to-bottom, then left-to-right, mirrored
    /// for a right-to-left control. Rows are split when a control starts more than
    /// <paramref name="rowTolerance"/> px below the previous row's top.
    /// </summary>
    internal static IReadOnlyList<Control> ReadingOrder(IReadOnlyList<Control> controls, Visual root, double rowTolerance = 6)
    {
        var placed = controls
            .Select(control => (Control: control, Bounds: BoundsIn(control, root)))
            .Where(item => item.Bounds is not null)
            .Select(item => (item.Control, Rect: item.Bounds!.Value))
            .OrderBy(item => item.Rect.Y)
            .ThenBy(item => item.Control.FlowDirection == FlowDirection.RightToLeft ? -item.Rect.Right : item.Rect.X)
            .ToList();
        var rows = new List<List<(Control Control, Rect Rect)>>();
        foreach (var item in placed)
        {
            if (rows.Count == 0 || item.Rect.Y > rows[^1][0].Rect.Y + rowTolerance) rows.Add([]);
            rows[^1].Add(item);
        }
        return rows
            .SelectMany(row => row.OrderBy(item => item.Control.FlowDirection == FlowDirection.RightToLeft ? -item.Rect.Right : item.Rect.X))
            .Select(item => item.Control)
            .ToList();
    }

    internal static string Describe(Control control)
    {
        ArgumentNullException.ThrowIfNull(control);
        var name = AccessibleName(control);
        return string.IsNullOrWhiteSpace(name) ? Role(control) : $"{Role(control)} \"{name}\"";
    }
}
