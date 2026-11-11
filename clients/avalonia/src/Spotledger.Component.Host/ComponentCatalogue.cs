namespace Spotledger.Component.Host;

/// <summary>
/// Where a host records what it refused and what it loaded, so a support engineer can read why a
/// package did not appear. The default writes nothing; the desktop shell passes its own.
/// <para>Nothing here is a secret: a refusal message names a file, a key id, a publisher or a
/// version, never key material.</para>
/// </summary>
public interface IComponentHostLog
{
    /// <summary>A package was refused, with its specific code.</summary>
    /// <param name="refusal">The refusal.</param>
    void Refused(ComponentRefusal refusal);

    /// <summary>A component was loaded and registered.</summary>
    /// <param name="component">The loaded component.</param>
    void Loaded(LoadedComponent component);

    /// <summary>A component was uninstalled and its context unloaded.</summary>
    /// <param name="componentId">The component's id.</param>
    /// <param name="version">The version that was unloaded.</param>
    void Unloaded(string componentId, string version);

    /// <summary>A component failed at run time and its slot shows a placeholder.</summary>
    /// <param name="refusal">The failure.</param>
    void Failed(ComponentRefusal refusal);

    /// <summary>An install or upgrade was recorded, with the actor of an allowed downgrade. Optional.</summary>
    /// <param name="entry">The install log entry.</param>
    void InstallRecorded(InstallLogEntry entry)
    {
        // A log that does not care about installs keeps the default.
    }

    /// <summary>A revocation list disabled an installed component. Optional.</summary>
    /// <param name="outcome">What happened to it.</param>
    void Revoked(RevocationOutcome outcome)
    {
        // A log that does not care keeps the default.
    }
}

/// <summary>A log that records nothing: the host that does not want a log at all.</summary>
public sealed class NullComponentHostLog : IComponentHostLog
{
    /// <summary>The single instance.</summary>
    public static NullComponentHostLog Instance { get; } = new();

    /// <inheritdoc />
    public void Refused(ComponentRefusal refusal)
    {
        // Deliberately inert.
    }

    /// <inheritdoc />
    public void Loaded(LoadedComponent component)
    {
        // Deliberately inert.
    }

    /// <inheritdoc />
    public void Unloaded(string componentId, string version)
    {
        // Deliberately inert.
    }

    /// <inheritdoc />
    public void Failed(ComponentRefusal refusal)
    {
        // Deliberately inert.
    }
}

/// <summary>
/// The catalogue of installed components: what an install adds, what an uninstall removes, and what
/// a view resolves a component id against. It is the host's own because the contract's
/// <see cref="Contract.ComponentRegistry"/> is append-only (it is the host's built-in set); the
/// catalogue is the part of the registry an install and an uninstall move.
/// <para>
/// One id has at most one installed version: an upgrade replaces the entry, and a second install of
/// the same version is refused as a duplicate.
/// </para>
/// </summary>
public sealed class ComponentCatalogue
{
    private readonly Dictionary<string, LoadedComponent> _byId = new(StringComparer.Ordinal);
    private readonly List<LoadedComponent> _ordered = new();

    /// <summary>The installed components, in install order.</summary>
    public IReadOnlyList<LoadedComponent> Components => _ordered;

    /// <summary>The number of installed components.</summary>
    public int Count => _ordered.Count;

    /// <summary>Adds a loaded component, refusing an id that is already installed.</summary>
    /// <param name="component">The component.</param>
    public ComponentRefusal? Add(LoadedComponent component)
    {
        ArgumentNullException.ThrowIfNull(component);
        if (_byId.TryGetValue(component.ComponentId, out var existing))
        {
            return new ComponentRefusal(
                "component.id.duplicate",
                "'" + component.ComponentId + "' is already installed at " + existing.Descriptor.Version.ToString()
                + "; uninstall or upgrade it instead of installing a second copy.");
        }

        _byId.Add(component.ComponentId, component);
        _ordered.Add(component);
        return null;
    }

    /// <summary>Looks an installed component up by id.</summary>
    /// <param name="componentId">The component id.</param>
    /// <param name="component">The component, when it is installed.</param>
    public bool TryResolve(string componentId, out LoadedComponent? component)
    {
        component = null;
        return componentId is not null && _byId.TryGetValue(componentId, out component);
    }

    /// <summary>Removes a component from the catalogue without disposing it.</summary>
    /// <param name="componentId">The component id.</param>
    /// <param name="component">The component that was installed, when it was.</param>
    internal bool Remove(string componentId, out LoadedComponent? component)
    {
        component = null;
        if (componentId is null || !_byId.Remove(componentId, out component))
        {
            return false;
        }

        _ordered.Remove(component);
        return true;
    }
}
