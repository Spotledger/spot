namespace Spotledger.Component.Contract;

/// <summary>A validation finding: a stable code a host may translate, and a message to show.</summary>
/// <param name="Code">Stable, dot-separated code (for example <c>rating.out_of_range</c>).</param>
/// <param name="Message">The message rendered to the user; already localized by the component.</param>
/// <param name="Severity">Whether the value is refused or merely flagged.</param>
public readonly record struct ValidationMessage(string Code, string Message, ValidationSeverity Severity = ValidationSeverity.Error)
{
    /// <summary>True when the message refuses the value.</summary>
    public bool IsError => Severity == ValidationSeverity.Error;
}

/// <summary>How seriously a <see cref="ValidationMessage"/> counts.</summary>
public enum ValidationSeverity
{
    /// <summary>The value is shown but flagged.</summary>
    Warning,

    /// <summary>The value is refused: the host does not commit it.</summary>
    Error,
}

/// <summary>
/// What assistive technology is told about a component. <see cref="AutomationName"/> is
/// mandatory: a component without a name fails the contract kit, because an unnamed control is
/// unusable with a screen reader.
/// </summary>
/// <param name="AutomationName">The accessible name; never empty.</param>
/// <param name="Role">The role assistive technology announces.</param>
/// <param name="HelpText">Optional hint read after the name.</param>
/// <param name="AutomationId">Optional stable id for UI automation scripts.</param>
/// <param name="LiveRegion">Whether validation or value changes are announced automatically.</param>
public sealed record ComponentAccessibility(
    string AutomationName,
    ComponentAutomationRole Role,
    string? HelpText = null,
    string? AutomationId = null,
    ComponentLiveRegion LiveRegion = ComponentLiveRegion.None)
{
    /// <summary>Everything wrong with this accessibility declaration; empty when it is sound.</summary>
    public IReadOnlyList<string> Validate()
    {
        var problems = new List<string>();
        if (string.IsNullOrWhiteSpace(AutomationName))
        {
            problems.Add("accessibility.name.missing: AutomationName is empty, so a screen reader cannot name the component.");
        }

        if (AutomationName is not null && AutomationName.Length > 120)
        {
            problems.Add("accessibility.name.too_long: AutomationName is longer than 120 characters.");
        }

        if (HelpText is not null && HelpText.Length > 400)
        {
            problems.Add("accessibility.help.too_long: HelpText is longer than 400 characters.");
        }

        return problems;
    }

    /// <summary>The wire name of the role, used by the kit and by host diagnostics.</summary>
    public string RoleName => Role.ToString();
}

/// <summary>The roles the contract allows a component to announce.</summary>
public enum ComponentAutomationRole
{
    /// <summary>An invokable control.</summary>
    Button,

    /// <summary>A two-state control.</summary>
    Toggle,

    /// <summary>A value picked along a range.</summary>
    Slider,

    /// <summary>A text entry.</summary>
    TextInput,

    /// <summary>A picker over options.</summary>
    Combo,

    /// <summary>A list of items.</summary>
    List,

    /// <summary>A table of rows.</summary>
    Table,

    /// <summary>A grouping element with no role of its own.</summary>
    Group,

    /// <summary>A read-only status readout.</summary>
    Status,
}

/// <summary>Whether changes are announced automatically, and how urgently.</summary>
public enum ComponentLiveRegion
{
    /// <summary>Nothing is announced automatically.</summary>
    None,

    /// <summary>Announced when the user pauses.</summary>
    Polite,

    /// <summary>Announced at once; reserved for validation errors.</summary>
    Assertive,
}

/// <summary>
/// The keys a component consumes. The contract is deliberately small: the host owns Tab
/// traversal, so a component never traps focus; it declares the keys it handles so the host can
/// keep accelerators out of its way.
/// </summary>
/// <param name="EntryKeys">Keys that move focus into the component from the host.</param>
/// <param name="ExitKeys">Keys that move focus out again.</param>
/// <param name="ActivationKeys">Keys that change the value.</param>
/// <param name="HandledKeys">Every key the component consumes (a superset of the three lists).</param>
/// <param name="TrapsTab">Must be false: a component that traps Tab breaks keyboard-only navigation.</param>
public sealed record KeyboardContract(
    IReadOnlyList<ComponentKey> EntryKeys,
    IReadOnlyList<ComponentKey> ExitKeys,
    IReadOnlyList<ComponentKey> ActivationKeys,
    IReadOnlyList<ComponentKey> HandledKeys,
    bool TrapsTab = false)
{
    /// <summary>Everything wrong with this keyboard declaration; empty when it is sound.</summary>
    public IReadOnlyList<string> Validate()
    {
        ArgumentNullException.ThrowIfNull(EntryKeys);
        ArgumentNullException.ThrowIfNull(ExitKeys);
        ArgumentNullException.ThrowIfNull(ActivationKeys);
        ArgumentNullException.ThrowIfNull(HandledKeys);
        var problems = new List<string>();
        if (EntryKeys.Count == 0)
        {
            problems.Add("keyboard.entry.missing: the component declares no way in.");
        }

        if (ActivationKeys.Count == 0)
        {
            problems.Add("keyboard.activation.missing: the component declares no key that changes its value.");
        }

        if (TrapsTab)
        {
            problems.Add("keyboard.tab.trapped: the host owns Tab traversal; a component never traps it.");
        }

        foreach (var key in ActivationKeys.Concat(ExitKeys).Concat(EntryKeys))
        {
            if (!HandledKeys.Contains(key))
            {
                problems.Add("keyboard.handled.missing: " + key + " is declared but not listed in HandledKeys.");
            }
        }

        return problems;
    }
}

/// <summary>The keys a component may name in its <see cref="KeyboardContract"/>.</summary>
public enum ComponentKey
{
    /// <summary>Tab: the host's traversal key.</summary>
    Tab,

    /// <summary>Enter: commit.</summary>
    Enter,

    /// <summary>Space: activate.</summary>
    Space,

    /// <summary>Escape: cancel.</summary>
    Escape,

    /// <summary>Left arrow.</summary>
    ArrowLeft,

    /// <summary>Right arrow.</summary>
    ArrowRight,

    /// <summary>Up arrow.</summary>
    ArrowUp,

    /// <summary>Down arrow.</summary>
    ArrowDown,

    /// <summary>Home: first item or minimum.</summary>
    Home,

    /// <summary>End: last item or maximum.</summary>
    End,

    /// <summary>Page up.</summary>
    PageUp,

    /// <summary>Page down.</summary>
    PageDown,

    /// <summary>Delete: clear the value.</summary>
    Delete,

    /// <summary>Backspace.</summary>
    Backspace,

    /// <summary>F2: begin editing.</summary>
    F2,
}

/// <summary>
/// The budget a component promises to keep. The kit measures 1,000 updates against it, so the
/// numbers here are what a slow component gets refused on, not decoration.
/// </summary>
/// <param name="BudgetMillisecondsPerUpdate">Average wall time one value update may take.</param>
/// <param name="MaxBytesAllocatedPerUpdate">Average managed bytes one value update may allocate.</param>
/// <param name="SupportsVirtualisation">True when the component renders only the visible slice of a large list.</param>
/// <param name="MaxVisibleItems">For a list-shaped component, the most items it renders at once.</param>
public sealed record PerformanceContract(
    int BudgetMillisecondsPerUpdate,
    int MaxBytesAllocatedPerUpdate,
    bool SupportsVirtualisation = false,
    int MaxVisibleItems = 0)
{
    /// <summary>Everything wrong with this performance declaration; empty when it is sound.</summary>
    public IReadOnlyList<string> Validate()
    {
        var problems = new List<string>();
        if (BudgetMillisecondsPerUpdate <= 0)
        {
            problems.Add("performance.budget.missing: BudgetMillisecondsPerUpdate must be positive.");
        }

        if (MaxBytesAllocatedPerUpdate <= 0)
        {
            problems.Add("performance.allocations.missing: MaxBytesAllocatedPerUpdate must be positive.");
        }

        if (SupportsVirtualisation && MaxVisibleItems <= 0)
        {
            problems.Add("performance.virtualisation.limit: a virtualising component declares MaxVisibleItems.");
        }

        return problems;
    }
}

/// <summary>The field a component is bound to, as the component sees it (no permission model here).</summary>
/// <param name="Name">The storage name.</param>
/// <param name="Label">The human label, already localized by the host.</param>
/// <param name="Type">The field type.</param>
/// <param name="Required">Whether an empty value is refused.</param>
/// <param name="ReadOnly">Whether the field is not editable.</param>
/// <param name="MaxChars">The text length limit, when the metadata carries one.</param>
/// <param name="Options">The select options, in order.</param>
/// <param name="HelpText">Optional help text, already localized.</param>
public sealed record FieldMetadata(
    string Name,
    string Label,
    ComponentFieldType Type,
    bool Required = false,
    bool ReadOnly = false,
    int? MaxChars = null,
    IReadOnlyList<string>? Options = null,
    string? HelpText = null)
{
    /// <summary>The options, never null.</summary>
    public IReadOnlyList<string> OptionsOrEmpty => Options ?? Array.Empty<string>();
}
