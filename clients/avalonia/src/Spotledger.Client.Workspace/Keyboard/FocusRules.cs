namespace Spotledger.Client.Workspace.Keyboard;

/// <summary>
/// The focus rules of the workspace, as a table: for each moment where focus could be lost, where it
/// goes and why. The table is the code; <c>FOCUS.md</c> is the same table for people, and the tests
/// in <c>Keyboard/FocusRuleTests</c> assert one rule each against the real arrangement.
/// <para>The two invariants behind every rule: focus never lands on a disabled or hidden control, and
/// a change nobody asked for never takes the focus away from where a person put it.</para>
/// </summary>
public enum FocusRule
{
    /// <summary>A row was opened: focus goes to the form, on its first field.</summary>
    AfterOpen,

    /// <summary>A save succeeded: focus stays in the form (the person keeps working).</summary>
    AfterSave,

    /// <summary>A delete succeeded: focus goes back to the list, on the next row.</summary>
    AfterDelete,

    /// <summary>A document was cancelled: focus goes back to the list.</summary>
    AfterCancel,

    /// <summary>An operation failed: focus goes to the form and the message is announced.</summary>
    AfterError,

    /// <summary>A dialog closed: focus returns to the control that opened it.</summary>
    AfterDialogClose,

    /// <summary>A pane was switched: focus goes to the pane's entry control.</summary>
    AfterPaneSwitch,

    /// <summary>A tab was switched: focus goes to the list (or form) of the tab that became active.</summary>
    AfterTabSwitch,

    /// <summary>A refresh finished: focus stays where it was and the new row count is announced.</summary>
    AfterRefresh,

    /// <summary>A validation failure on save: focus goes to the first invalid field, scrolled into view,
    /// and the message is announced.</summary>
    AfterValidationFailure,
}

/// <summary>What the arrangement knows when it applies a rule.</summary>
public sealed record FocusContext
{
    /// <summary>The automation id of the control that had the focus before the change.</summary>
    public string? Previous { get; init; }

    /// <summary>The automation id of the pane that just became active (pane and tab switches).</summary>
    public string? Pane { get; init; }

    /// <summary>The storage name of the first invalid field, when the server named one.</summary>
    public string? InvalidField { get; init; }

    /// <summary>The message to announce, when there is one.</summary>
    public string? Message { get; init; }
}

/// <summary>Where a rule sends the focus.</summary>
/// <param name="Rule">The rule that produced it.</param>
/// <param name="AutomationId">The target control's automation id.</param>
/// <param name="Announcement">What a screen reader must hear, or null.</param>
/// <param name="ScrollIntoView">True for a validation failure: the target must be brought into view.</param>
public sealed record FocusTarget(FocusRule Rule, string AutomationId, string? Announcement, bool ScrollIntoView = false);

/// <summary>The focus rule table.</summary>
public static class FocusRules
{
    /// <summary>The automation id of the list pane.</summary>
    public const string List = "split.list";

    /// <summary>The automation id of the form pane.</summary>
    public const string Form = "split.form";

    /// <summary>The automation id of the form's message area.</summary>
    public const string Messages = "split.form.messages";

    /// <summary>The automation id of the unsaved-changes guard.</summary>
    public const string Guard = "split.guard";

    /// <summary>The automation id of the splitter.</summary>
    public const string Splitter = "split.splitter";

    /// <summary>The automation id of a form field's control.</summary>
    public static string Field(string name) => FocusTrace.FieldPrefix + name;

    /// <summary>Resolves where focus goes for <paramref name="rule"/>.</summary>
    public static FocusTarget Resolve(FocusRule rule, FocusContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        return rule switch
        {
            FocusRule.AfterOpen => new FocusTarget(rule, Form, null),
            FocusRule.AfterSave => new FocusTarget(rule, context.Previous ?? Form, "Saved"),
            FocusRule.AfterDelete => new FocusTarget(rule, List, context.Message ?? "Deleted"),
            FocusRule.AfterCancel => new FocusTarget(rule, List, context.Message ?? "Cancelled"),
            FocusRule.AfterError => new FocusTarget(
                rule,
                context.InvalidField is { Length: > 0 } field ? Field(field) : Form,
                context.Message ?? "The operation failed"),
            FocusRule.AfterDialogClose => new FocusTarget(rule, context.Previous ?? Form, null),
            FocusRule.AfterPaneSwitch => new FocusTarget(rule, context.Pane ?? List, null),
            FocusRule.AfterTabSwitch => new FocusTarget(rule, context.Pane ?? List, null),
            FocusRule.AfterRefresh => new FocusTarget(rule, context.Previous ?? List, context.Message),
            FocusRule.AfterValidationFailure => new FocusTarget(
                rule,
                Field(context.InvalidField ?? string.Empty),
                context.Message ?? "The form has an error",
                ScrollIntoView: true),
            _ => throw new ArgumentOutOfRangeException(nameof(rule), rule, "Unknown focus rule."),
        };
    }
}
