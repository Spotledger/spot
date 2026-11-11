using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.VisualTree;

namespace Spotledger.Client.Workspace.Keyboard;

/// <summary>What a <see cref="FocusTrace"/> entry records.</summary>
public enum FocusKind
{
    /// <summary>A control received the focus.</summary>
    Focus,

    /// <summary>A screen-reader live region was spoken to.</summary>
    Announcement,

    /// <summary>Focus landed on a control that is disabled or hidden: a rule violation.</summary>
    Violation,

    /// <summary>A rule was applied (the arrangement moved the focus on purpose).</summary>
    Rule,
}

/// <summary>One recorded event.</summary>
/// <param name="Kind">What happened.</param>
/// <param name="AutomationId">The target's automation id (<c>split.form</c>, <c>field:title</c>, …).</param>
/// <param name="Note">A rule name or the announced text.</param>
public sealed record FocusEntry(FocusKind Kind, string AutomationId, string Note);

/// <summary>
/// The focus trace of a window: every real focus change, every announcement and every rule the
/// arrangement applied, in order. It is attached by the tests (a host may attach it behind a
/// diagnostic flag) and it is the only thing the workflow harness reads to know where focus went:
/// the entry comes from Avalonia's own <see cref="InputElement.GotFocusEvent"/>, not from what the
/// view meant to do.
/// </summary>
public sealed class FocusTrace
{
    /// <summary>The prefix every automation id of this project uses for a form field.</summary>
    public const string FieldPrefix = "field:";

    private readonly List<FocusEntry> _entries = [];

    /// <summary>Every recorded event, oldest first.</summary>
    public IReadOnlyList<FocusEntry> Entries => _entries;

    /// <summary>The automation id of the control that holds the focus, or null when none does.</summary>
    public string? Current { get; private set; }

    /// <summary>What the last live region said.</summary>
    public string? LastAnnouncement { get; private set; }

    /// <summary>Every focus change that landed on a disabled or hidden control.</summary>
    public IReadOnlyList<FocusEntry> Violations => [.. _entries.Where(entry => entry.Kind == FocusKind.Violation)];

    /// <summary>Starts tracing the focus of <paramref name="root"/>'s window.</summary>
    public void Attach(InputElement root)
    {
        ArgumentNullException.ThrowIfNull(root);
        root.AddHandler(InputElement.GotFocusEvent, OnGotFocus, RoutingStrategies.Bubble, handledEventsToo: true);
    }

    /// <summary>Stops tracing.</summary>
    public void Detach(InputElement root)
    {
        ArgumentNullException.ThrowIfNull(root);
        root.RemoveHandler(InputElement.GotFocusEvent, OnGotFocus);
    }

    /// <summary>Records what a live region said.</summary>
    public void Announce(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        LastAnnouncement = text;
        _entries.Add(new FocusEntry(FocusKind.Announcement, Current ?? string.Empty, text));
    }

    /// <summary>Records that a focus rule was applied and where it put the focus.</summary>
    public void Rule(string rule, string automationId)
        => _entries.Add(new FocusEntry(FocusKind.Rule, automationId, rule));

    /// <summary>True when focus has been on <paramref name="automationId"/> at any point.</summary>
    public bool SawFocus(string automationId)
        => _entries.Exists(entry => entry.Kind == FocusKind.Focus && string.Equals(entry.AutomationId, automationId, StringComparison.Ordinal));

    /// <summary>Forgets everything (a test asks "where did the *next* key put the focus").</summary>
    public void Clear()
    {
        _entries.Clear();
        LastAnnouncement = null;
    }

    /// <summary>The automation id of <paramref name="element"/> or of its nearest named ancestor.</summary>
    public static string? IdOf(Visual? element)
    {
        for (var visual = element; visual is not null; visual = visual.GetVisualParent())
        {
            var id = AutomationProperties.GetAutomationId(visual);
            if (!string.IsNullOrEmpty(id))
            {
                return id;
            }
        }

        return null;
    }

    private void OnGotFocus(object? sender, FocusChangedEventArgs e)
    {
        var id = IdOf(e.Source as Visual) ?? string.Empty;
        Current = id;
        _entries.Add(new FocusEntry(FocusKind.Focus, id, string.Empty));
        if (e.Source is Control control && (!control.IsEffectivelyEnabled || !control.IsEffectivelyVisible))
        {
            _entries.Add(new FocusEntry(FocusKind.Violation, id, "focus landed on a disabled or hidden control"));
        }
    }
}
