using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.VisualTree;

namespace Spotledger.Client.Workspace.Keyboard;

/// <summary>
/// The pane ring of a window: F6 moves to the next pane, Shift+F6 to the previous one, wrapping, and
/// a pane that is hidden or disabled is skipped rather than focused. The window's host registers the
/// panes it has (the chrome registers the menu, the sidebar, the tabs and the status line; the split
/// arrangement registers its list and its form), so the ring never claims a pane that is not there.
/// </summary>
public sealed class PaneRing
{
    private readonly List<(string Id, Control Control)> _panes = [];

    /// <summary>The registered pane ids, in cycle order.</summary>
    public IReadOnlyList<string> Order => [.. _panes.Select(pane => pane.Id)];

    /// <summary>Adds a pane at the end of the ring; the same id twice is an error.</summary>
    public void Add(string id, Control control)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(id);
        ArgumentNullException.ThrowIfNull(control);
        if (_panes.Exists(pane => string.Equals(pane.Id, id, StringComparison.Ordinal)))
        {
            throw new InvalidOperationException($"Pane '{id}' is already in the ring.");
        }

        _panes.Add((id, control));
    }

    /// <summary>The id of the pane that holds the focus, or null.</summary>
    public string? FocusedPane
    {
        get
        {
            var focused = TopLevel.GetTopLevel(_panes.Count > 0 ? _panes[0].Control : null)?.FocusManager?.GetFocusedElement();
            if (focused is not Visual visual)
            {
                return null;
            }

            foreach (var (id, control) in _panes)
            {
                if (ReferenceEquals(visual, control) || control.IsVisualAncestorOf(visual) || ReferenceEquals(visual, control))
                {
                    return id;
                }
            }

            return null;
        }
    }

    /// <summary>Focuses the next (<paramref name="step"/> = 1) or previous (-1) pane that can take it.</summary>
    public bool Cycle(int step)
    {
        if (_panes.Count == 0)
        {
            return false;
        }

        var current = IndexOf(FocusedPane);
        for (var offset = 1; offset <= _panes.Count; offset++)
        {
            var index = ((current + (step * offset)) % _panes.Count + _panes.Count) % _panes.Count;
            if (TryFocus(_panes[index].Control))
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>Focuses one named pane; false when it is absent or cannot take the focus.</summary>
    public bool Focus(string id)
    {
        foreach (var (paneId, control) in _panes)
        {
            if (string.Equals(paneId, id, StringComparison.Ordinal))
            {
                return TryFocus(control);
            }
        }

        return false;
    }

    /// <summary>True when the pane can take the focus right now (visible, enabled, or holding one such child).</summary>
    public static bool CanFocus(Control control)
    {
        ArgumentNullException.ThrowIfNull(control);
        return Entry(control) is not null;
    }

    /// <summary>The control a pane hands the focus to: itself, or its first focusable descendant.</summary>
    public static Control? Entry(Control control)
    {
        ArgumentNullException.ThrowIfNull(control);
        if (!control.IsEffectivelyVisible || !control.IsEffectivelyEnabled)
        {
            return null;
        }

        if (control.Focusable)
        {
            return control;
        }

        return DepthFirst(control);
    }

    private static Control? DepthFirst(Control control)
    {
        foreach (var child in control.GetVisualChildren().OfType<Control>())
        {
            if (!child.IsEffectivelyVisible || !child.IsEffectivelyEnabled)
            {
                continue;
            }

            if (child.Focusable)
            {
                return child;
            }

            if (DepthFirst(child) is { } nested)
            {
                return nested;
            }
        }

        return null;
    }

    private static bool TryFocus(Control control) => Entry(control)?.Focus() ?? false;

    private int IndexOf(string? id)
    {
        if (id is null)
        {
            return -1;
        }

        var index = _panes.FindIndex(pane => string.Equals(pane.Id, id, StringComparison.Ordinal));
        return index < 0 ? -1 : index;
    }
}
