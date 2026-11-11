using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>A component failure the host has seen and turned into a visible placeholder.</summary>
/// <param name="ComponentId">The component's id.</param>
/// <param name="ComponentVersion">The component's version.</param>
/// <param name="Refusal">What went wrong, with its stable code.</param>
public sealed record ComponentSlotError(string ComponentId, string ComponentVersion, ComponentRefusal Refusal);

/// <summary>
/// One form slot's view: either the component's control, or a placeholder for a component that threw
/// or overran its budget. The form always gets a control it can lay out, so a broken component never
/// takes the document down with it.
/// </summary>
public sealed class ComponentSlotView
{
    internal ComponentSlotView(Control view, IComponent? component, ComponentInstance? instance, ComponentSlotError? error)
    {
        View = view;
        Component = component;
        Instance = instance;
        Error = error;
    }

    /// <summary>The control to put in the slot: the component's, or the placeholder.</summary>
    public Control View { get; }

    /// <summary>The live component; null when the slot shows a placeholder.</summary>
    public IComponent? Component { get; }

    /// <summary>The tracked instance, when the component came from an installed package.</summary>
    public ComponentInstance? Instance { get; }

    /// <summary>The failure; null when the component is live.</summary>
    public ComponentSlotError? Error { get; }

    /// <summary>True when the slot shows the placeholder.</summary>
    public bool IsPlaceholder => Component is null;
}

/// <summary>
/// Failure isolation for tier B components. Creating a component, setting its value and disposing it
/// are calls into third-party code, and any of them can throw; this slot turns a throw into an error
/// entry and a placeholder control rather than an exception that unwinds the form.
/// <para>
/// It is isolation of <em>failures</em>, not of code: the component runs in this process. A component
/// that corrupts state or hangs a thread is still in-process; see <see cref="ComponentResourceGuard"/>
/// for what the guard does and does not do.
/// </para>
/// </summary>
public sealed class ComponentSlot
{
    private readonly ComponentResourceGuard _guard;
    private readonly IComponentHostLog _log;
    private readonly List<ComponentSlotError> _errors = new();

    /// <summary>Creates the slot of one form.</summary>
    /// <param name="guard">The resource guard the constructor and updates run under.</param>
    /// <param name="log">Where failures are recorded.</param>
    public ComponentSlot(ComponentResourceGuard guard, IComponentHostLog? log = null)
    {
        _guard = guard ?? throw new ArgumentNullException(nameof(guard));
        _log = log ?? NullComponentHostLog.Instance;
    }

    /// <summary>Every failure this slot has shown, oldest first.</summary>
    public IReadOnlyList<ComponentSlotError> Errors => _errors;

    /// <summary>
    /// Creates an installed component for a slot, within its constructor budget and behind a catch: a
    /// component that throws or overruns yields a placeholder and an error entry.
    /// </summary>
    /// <param name="component">The loaded component.</param>
    /// <param name="context">The context to create it with.</param>
    /// <param name="callbacks">The host's handlers.</param>
    public ComponentSlotView Open(LoadedComponent component, ComponentContext context, ComponentCallbacks? callbacks = null)
    {
        ArgumentNullException.ThrowIfNull(component);
        ArgumentNullException.ThrowIfNull(context);
        return Open(component.Descriptor, context, callbacks, component);
    }

    /// <summary>
    /// Creates a host-built component for a slot, under the same guard and the same catch. A built-in
    /// can fail like any other component, and a form shows the same placeholder when it does.
    /// </summary>
    /// <param name="descriptor">The descriptor.</param>
    /// <param name="context">The context to create it with.</param>
    /// <param name="callbacks">The host's handlers; ignored for a built-in, which has no tracked instance.</param>
    public ComponentSlotView Open(
        ComponentDescriptor descriptor,
        ComponentContext context,
        ComponentCallbacks? callbacks = null)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        ArgumentNullException.ThrowIfNull(context);
        return Open(descriptor, context, callbacks, null);
    }

    /// <summary>
    /// Sets the value of a live component within its update budget, behind a catch: a throw is an
    /// error entry and the slot keeps showing the component's last good value.
    /// </summary>
    /// <param name="view">The slot's view.</param>
    /// <param name="value">The value from the document.</param>
    public GuardedOutcome<bool>? Update(ComponentSlotView view, ComponentValue value)
    {
        ArgumentNullException.ThrowIfNull(view);
        if (view.Component is null)
        {
            return null;
        }

        var component = view.Component;
        ComponentRefusal? failure = null;
#pragma warning disable CA1031 // An update that throws is an error entry, not a crashed form.
        try
        {
            var outcome = _guard.RunUpdate(component.Descriptor.Id, () => component.SetValue(value));
            if (outcome.Refusal is not null)
            {
                Record(new ComponentSlotError(component.Descriptor.Id, component.Descriptor.Version.ToString(), outcome.Refusal));
            }

            return outcome;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            failure = new ComponentRefusal(
                ComponentRefusalCodes.ComponentThrew,
                "an update of '" + component.Descriptor.Id + "' threw " + error.GetType().Name + ": " + error.Message);
        }
#pragma warning restore CA1031

        Record(new ComponentSlotError(component.Descriptor.Id, component.Descriptor.Version.ToString(), failure));
        return new GuardedOutcome<bool>(false, 0, failure);
    }

    private ComponentSlotView Open(
        ComponentDescriptor descriptor,
        ComponentContext context,
        ComponentCallbacks? callbacks,
        LoadedComponent? owner)
    {
        ComponentInstance? instance = null;
        IComponent? component = null;
        ComponentRefusal? failure = null;
#pragma warning disable CA1031 // A component's exception is data here: the form shows a placeholder instead of dying.
        try
        {
            var outcome = owner is null
                ? _guard.RunConstructor(descriptor.Id, () => (object)descriptor.Create(context))
                : _guard.RunConstructor(descriptor.Id, () => (object)owner.Create(context, callbacks));
            if (outcome.Refusal is not null)
            {
                failure = outcome.Refusal;
            }
            else if (outcome.Value is ComponentInstance tracked)
            {
                instance = tracked;
                component = tracked.Component;
            }
            else
            {
                component = outcome.Value as IComponent;
            }
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            failure = new ComponentRefusal(
                ComponentRefusalCodes.ComponentThrew,
                "the constructor of '" + descriptor.Id + "' threw " + error.GetType().Name + ": " + error.Message);
        }
#pragma warning restore CA1031

        if (failure is not null || component is null)
        {
            instance?.Dispose();
            failure ??= new ComponentRefusal(
                ComponentRefusalCodes.ComponentThrew,
                "the constructor of '" + descriptor.Id + "' produced no component.");
            return Placeholder(descriptor, failure);
        }

        return new ComponentSlotView(component.View, component, instance, null);
    }

    private ComponentSlotView Placeholder(ComponentDescriptor descriptor, ComponentRefusal failure)
    {
        var error = new ComponentSlotError(descriptor.Id, descriptor.Version.ToString(), failure);
        Record(error);
        return new ComponentSlotView(BuildPlaceholder(descriptor.Id, failure.Code), null, null, error);
    }

    private void Record(ComponentSlotError error)
    {
        _errors.Add(error);
        _log.Failed(error.Refusal);
    }

    /// <summary>The control a failed component's slot shows: a border with the reason's code.</summary>
    /// <param name="componentId">The component's id.</param>
    /// <param name="code">The stable refusal code.</param>
    public static Control BuildPlaceholder(string componentId, string code) =>
        new Border
        {
            Padding = new Thickness(8),
            HorizontalAlignment = HorizontalAlignment.Stretch,
            Child = new TextBlock
            {
                Text = "Component " + componentId + " is unavailable (" + code + ").",
                TextWrapping = TextWrapping.Wrap,
            },
        };
}
