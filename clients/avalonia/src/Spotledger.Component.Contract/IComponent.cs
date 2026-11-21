using Avalonia.Controls;

namespace Spotledger.Component.Contract;

/// <summary>
/// A native component (tier B): it is handed a <see cref="ComponentContext"/> and renders one
/// Avalonia <see cref="Control"/>. Everything it may do is on this interface — value, events,
/// validation, accessibility, keyboard and performance declarations. The contract hands it no
/// access to host internals, the network, the file system or reflection. That is a statement about
/// what the API grants, not a sandbox: an in-process tier B component is trusted code, so a
/// bundle is reviewed and signed before it loads (T2), and the contract is not a security boundary.
/// </summary>
public interface IComponent : IDisposable
{
    /// <summary>The descriptor this instance was created from.</summary>
    ComponentDescriptor Descriptor { get; }

    /// <summary>The control the host puts in a form slot. Created once, never re-parented by the component.</summary>
    Control View { get; }

    /// <summary>The current value.</summary>
    ComponentValue Value { get; }

    /// <summary>
    /// Sets the value from the host (including from a server refresh). Must not raise
    /// <see cref="ValueChanged"/> — that event is for user edits only, so the host never loops.
    /// </summary>
    void SetValue(ComponentValue value);

    /// <summary>Raised when the user changes the value.</summary>
    event EventHandler<ComponentValueChangedEventArgs>? ValueChanged;

    /// <summary>Raised when the user commits the value (Enter, blur after a change).</summary>
    event EventHandler<ComponentCommittedEventArgs>? Committed;

    /// <summary>Raised when the user cancels (Escape): the host restores the last committed value.</summary>
    event EventHandler<ComponentCancelledEventArgs>? Cancelled;

    /// <summary>Raised after validation, with the messages the host should show.</summary>
    event EventHandler<ComponentValidatedEventArgs>? Validated;

    /// <summary>
    /// The validation hook: pure, no clock, no I/O. Must return an empty list for a valid value
    /// and at least one error for a value the field refuses.
    /// </summary>
    IReadOnlyList<ValidationMessage> Validate(ComponentValue proposed);

    /// <summary>What assistive technology is told.</summary>
    ComponentAccessibility Accessibility { get; }

    /// <summary>The keys this component consumes.</summary>
    KeyboardContract Keyboard { get; }

    /// <summary>The render budget this component promises.</summary>
    PerformanceContract Performance { get; }
}

/// <summary>A user-driven value change.</summary>
/// <param name="value">The new value.</param>
/// <param name="previous">The value before the change.</param>
public sealed class ComponentValueChangedEventArgs(ComponentValue value, ComponentValue previous) : EventArgs
{
    /// <summary>The new value.</summary>
    public ComponentValue Value { get; } = value;

    /// <summary>The value before the change.</summary>
    public ComponentValue Previous { get; } = previous;
}

/// <summary>A commit.</summary>
/// <param name="value">The committed value.</param>
public sealed class ComponentCommittedEventArgs(ComponentValue value) : EventArgs
{
    /// <summary>The committed value.</summary>
    public ComponentValue Value { get; } = value;
}

/// <summary>A cancellation.</summary>
/// <param name="value">The value the component held when the user cancelled.</param>
public sealed class ComponentCancelledEventArgs(ComponentValue value) : EventArgs
{
    /// <summary>The value at cancellation time.</summary>
    public ComponentValue Value { get; } = value;
}

/// <summary>The result of a validation pass.</summary>
/// <param name="messages">Every message, errors and warnings.</param>
public sealed class ComponentValidatedEventArgs(IReadOnlyList<ValidationMessage> messages) : EventArgs
{
    /// <summary>Every message.</summary>
    public IReadOnlyList<ValidationMessage> Messages { get; } = messages;

    /// <summary>True when any message refuses the value.</summary>
    public bool HasErrors => Messages.Any(static m => m.IsError);
}
