using System.Globalization;
using System.Reflection;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>
/// A component package that has passed verification and whose assemblies are loaded: the descriptor
/// it exposes, the facts the signature established, and the one platform component instance the host
/// may create from it. Disposing it disposes every live instance, drops every callback the host
/// registered and unloads the context; the package's code is collectable from then on.
/// <para>
/// The contract's <see cref="IComponent"/> is the only thing a caller gets hold of: never an
/// assembly, a type or a delegate into the package, because those are what keep a collectible
/// context alive.
/// </para>
/// </summary>
public sealed class LoadedComponent : IDisposable
{
    private readonly List<ComponentInstance> _instances = new();
    private readonly object _gate = new();
    private bool _disposed;

    internal LoadedComponent(
        ComponentPackage package,
        VerifiedPackage verified,
        ComponentDescriptor descriptor,
        ComponentLoadContext context)
    {
        Package = package;
        Verified = verified;
        Descriptor = descriptor;
        Context = context;
    }

    /// <summary>The package the component came from.</summary>
    public ComponentPackage Package { get; }

    /// <summary>The facts verification established.</summary>
    public VerifiedPackage Verified { get; }

    /// <summary>The descriptor the package exposes.</summary>
    public ComponentDescriptor Descriptor { get; }

    /// <summary>The component's id.</summary>
    public string ComponentId => Descriptor.Id;

    /// <summary>The collectible context the package's assemblies live in (lifetime, not security).</summary>
    public ComponentLoadContext Context { get; }

    /// <summary>The assembly names the context refused to resolve because they are the application's.</summary>
    public IReadOnlyList<string> RefusedAssemblies => Context.RefusedAssemblies;

    /// <summary>The instances this component has created and not yet disposed.</summary>
    public IReadOnlyList<ComponentInstance> Instances
    {
        get
        {
            lock (_gate)
            {
                return _instances.ToArray();
            }
        }
    }

    /// <summary>True after <see cref="Dispose"/> or a successful <see cref="Disable"/>.</summary>
    public bool IsUnloaded { get; private set; }

    /// <summary>Why the component was disabled (a revocation); null while it is enabled.</summary>
    public ComponentRefusal? DisabledBecause { get; private set; }

    /// <summary>True after <see cref="Disable"/>: no new instance is created, whether or not the code could be unloaded.</summary>
    public bool IsDisabled => DisabledBecause is not null;

    /// <summary>
    /// Creates a component instance through the descriptor factory. The instance is tracked, so an
    /// uninstall disposes it even if the caller forgets.
    /// </summary>
    /// <param name="context">The context the descriptor is handed.</param>
    /// <param name="callbacks">The host's handlers, which the instance unsubscribes on dispose.</param>
    public ComponentInstance Create(ComponentContext context, ComponentCallbacks? callbacks = null)
    {
        if (!TryCreate(context, callbacks, out var instance, out var refusal) || instance is null)
        {
            throw new InvalidOperationException(refusal?.ToString() ?? "the component could not create an instance.");
        }

        return instance;
    }

    /// <summary>
    /// Creates an instance, or says why not: a disabled (revoked) or disposed component creates
    /// nothing, and the refusal is accessible text for the UI. A failure inside the component's own
    /// factory still propagates, as it does from <see cref="Create"/>; the slot's guard handles that.
    /// </summary>
    /// <param name="context">The context the descriptor is handed.</param>
    /// <param name="callbacks">The host's handlers, which the instance unsubscribes on dispose.</param>
    /// <param name="instance">The instance, when one was created.</param>
    /// <param name="refusal">Why none was created.</param>
    public bool TryCreate(
        ComponentContext context,
        ComponentCallbacks? callbacks,
        out ComponentInstance? instance,
        out ComponentRefusal? refusal)
    {
        ArgumentNullException.ThrowIfNull(context);
        instance = null;
        refusal = null;
        if (DisabledBecause is { } disabled)
        {
            refusal = new ComponentRefusal(
                ComponentRefusalCodes.RevokedDisabled,
                "'" + ComponentId + "' was disabled and creates no new instance (" + disabled.Message + ")");
            return false;
        }

        if (_disposed)
        {
            refusal = new ComponentRefusal(
                ComponentRefusalCodes.ComponentUnknown,
                "'" + ComponentId + "' has been uninstalled.");
            return false;
        }

        var component = Descriptor.Create(context);
        instance = new ComponentInstance(this, component, callbacks ?? ComponentCallbacks.None);
        lock (_gate)
        {
            _instances.Add(instance);
        }

        return true;
    }

    /// <summary>
    /// Disables the component because a revocation list withdrew it: no new instance is created from
    /// now on, every live instance is disposed and the context is unloaded where the runtime allows
    /// it. This does NOT make the code safe to have run (it ran in this process with its full
    /// authority); it stops further use.
    /// </summary>
    /// <param name="because">The revocation that applies.</param>
    /// <returns>True when the code was unloaded; false when the process must restart to be rid of it.</returns>
    public bool Disable(ComponentRefusal because)
    {
        ArgumentNullException.ThrowIfNull(because);
        DisabledBecause = because;
        var unloaded = true;
        ComponentInstance[] live;
        lock (_gate)
        {
            live = _instances.ToArray();
            _instances.Clear();
        }

        foreach (var instance in live)
        {
            try
            {
                instance.Dispose();
            }
            catch (Exception error) when (error is not OutOfMemoryException)
            {
                unloaded = false;
            }
        }

        try
        {
            Context.Unload();
        }
        catch (InvalidOperationException)
        {
            unloaded = false;
        }

        _disposed = true;
        IsUnloaded = unloaded;
        return unloaded;
    }

    /// <summary>Disposes every live instance, drops the registrations and unloads the context.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        ComponentInstance[] live;
        lock (_gate)
        {
            live = _instances.ToArray();
            _instances.Clear();
        }

        foreach (var instance in live)
        {
            instance.Dispose();
        }

        Context.Unload();
        IsUnloaded = true;
    }

    internal void Forget(ComponentInstance instance)
    {
        lock (_gate)
        {
            _instances.Remove(instance);
        }
    }

    /// <inheritdoc />
    public override string ToString() =>
        Descriptor.Id + "@" + Descriptor.Version.ToString() + " ("
        + Verified.KeyId + ", signed (claimed by the publisher) " + Verified.SignedAt.ToString(CultureInfo.InvariantCulture) + ")";
}

/// <summary>The host's handlers for one component instance: value edits, commits, cancels, validation.</summary>
public sealed record ComponentCallbacks(
    Action<ComponentValue>? ValueChanged = null,
    Action<ComponentValue>? Committed = null,
    Action<ComponentValue>? Cancelled = null,
    Action<IReadOnlyList<ValidationMessage>>? Validated = null)
{
    /// <summary>A set that does nothing: the host that only renders.</summary>
    public static ComponentCallbacks None { get; } = new();
}

/// <summary>
/// One live component: the contract instance, its control and its value. The host creates it
/// through <see cref="LoadedComponent.Create"/>, hands <see cref="View"/> to a form slot and calls
/// <see cref="SetValue"/> when the document changes. <see cref="Dispose"/> unsubscribes the host's
/// callbacks before it disposes the component, which is what lets the collectible context go.
/// </summary>
public sealed class ComponentInstance : IDisposable
{
    private readonly LoadedComponent _owner;
    private readonly IComponent _component;
    private readonly ComponentCallbacks _callbacks;
    private EventHandler<ComponentValueChangedEventArgs>? _valueChanged;
    private EventHandler<ComponentCommittedEventArgs>? _committed;
    private EventHandler<ComponentCancelledEventArgs>? _cancelled;
    private EventHandler<ComponentValidatedEventArgs>? _validated;
    private bool _disposed;

    internal ComponentInstance(LoadedComponent owner, IComponent component, ComponentCallbacks callbacks)
    {
        _owner = owner;
        _component = component;
        _callbacks = callbacks;
        if (callbacks.ValueChanged is not null)
        {
            _valueChanged = (_, args) => callbacks.ValueChanged(args.Value);
            component.ValueChanged += _valueChanged;
        }

        if (callbacks.Committed is not null)
        {
            _committed = (_, args) => callbacks.Committed(args.Value);
            component.Committed += _committed;
        }

        if (callbacks.Cancelled is not null)
        {
            _cancelled = (_, args) => callbacks.Cancelled(args.Value);
            component.Cancelled += _cancelled;
        }

        if (callbacks.Validated is not null)
        {
            _validated = (_, args) => callbacks.Validated(args.Messages);
            component.Validated += _validated;
        }
    }

    /// <summary>The component.</summary>
    public IComponent Component => _component;

    /// <summary>The control the host puts in the slot.</summary>
    public Avalonia.Controls.Control View => _component.View;

    /// <summary>The component's current value.</summary>
    public ComponentValue Value => _component.Value;

    /// <summary>True after <see cref="Dispose"/>.</summary>
    public bool IsDisposed => _disposed;

    /// <summary>Sets the value from the host (a server refresh), which raises no user-edit event.</summary>
    /// <param name="value">The new value.</param>
    public void SetValue(ComponentValue value) => _component.SetValue(value);

    /// <summary>The component's own validation of a proposed value.</summary>
    /// <param name="proposed">The value to check.</param>
    public IReadOnlyList<ValidationMessage> Validate(ComponentValue proposed) => _component.Validate(proposed);

    /// <summary>Unsubscribes the host's callbacks, then disposes the component.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        if (_valueChanged is not null)
        {
            _component.ValueChanged -= _valueChanged;
            _valueChanged = null;
        }

        if (_committed is not null)
        {
            _component.Committed -= _committed;
            _committed = null;
        }

        if (_cancelled is not null)
        {
            _component.Cancelled -= _cancelled;
            _cancelled = null;
        }

        if (_validated is not null)
        {
            _component.Validated -= _validated;
            _validated = null;
        }

        _component.Dispose();
        _owner.Forget(this);
    }
}
