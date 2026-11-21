using System.Globalization;
using System.Runtime.CompilerServices;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>The outcome of an uninstall: whether anything was removed, and how much was torn down.</summary>
/// <param name="Removed">True when the component was installed and is now gone.</param>
/// <param name="DisposedInstances">How many live component instances were disposed.</param>
/// <param name="Refusal">Why nothing was removed, when the id was not installed.</param>
public sealed record ComponentUninstallResult(bool Removed, int DisposedInstances, ComponentRefusal? Refusal)
{
    /// <summary>A completed uninstall.</summary>
    public static ComponentUninstallResult Done(int disposedInstances) => new(true, disposedInstances, null);

    /// <summary>A refusal.</summary>
    public static ComponentUninstallResult Refuse(ComponentRefusal refusal) =>
        new(false, 0, refusal ?? throw new ArgumentNullException(nameof(refusal)));
}

/// <summary>
/// Install, upgrade and uninstall of component packages, with the teardown each one owes.
/// <list type="bullet">
/// <item><b>Install</b> reads the package, verifies it, loads it into a collectible context, adds it to
/// the catalogue and logs the acceptance. A refusal changes nothing.</item>
/// <item><b>Upgrade</b> verifies and loads the new package first, then disposes and removes the
/// installed version and registers the new one: a package that fails verification leaves the
/// working version in place.</item>
/// <item><b>Uninstall</b> disposes every live instance, which unsubscribes the host's callbacks,
/// clears the catalogue entry and unloads the context. The package's assemblies become collectable:
/// the test proves it with a weak reference.</item>
/// </list>
/// </summary>
public sealed class ComponentInstallationService
{
    private readonly ComponentTrustStore _trust;
    private readonly IComponentHostLog _log;
    private readonly IComponentStateStore _stateStore;
    private readonly ContractVersion _hostVersion;
    private readonly ComponentLoader _loader;
    private readonly List<RevocationOutcome> _outcomes = new();
    private readonly List<DisabledComponent> _unsavedDisabled = new();

    /// <summary>Creates the service over a trust store.</summary>
    /// <param name="trust">The trust material the store registry supplied.</param>
    /// <param name="log">Where refusals, loads and unloads are recorded.</param>
    /// <param name="hostVersion">The contract version this host speaks; the contract's when null.</param>
    /// <param name="stateStore">Where the install state (highest version, revocations applied, install log) persists; in memory when null.</param>
    public ComponentInstallationService(
        ComponentTrustStore trust,
        IComponentHostLog? log = null,
        ContractVersion? hostVersion = null,
        IComponentStateStore? stateStore = null)
    {
        _trust = trust ?? throw new ArgumentNullException(nameof(trust));
        _log = log ?? NullComponentHostLog.Instance;
        _stateStore = stateStore ?? new InMemoryComponentStateStore();
        _hostVersion = hostVersion ?? ComponentContract.Version;
        _loader = new ComponentLoader(_trust, Catalogue, _log, _hostVersion);
        _trust.RevocationAccepted += (_, _) => RecheckInstalled();
    }

    /// <summary>The installed components.</summary>
    public ComponentCatalogue Catalogue { get; } = new();

    /// <summary>
    /// True when the process must restart: a revoked component's code could not be unloaded, a
    /// revoked component had run in this process (its threads are not proven stopped), or a
    /// revocation could not be saved. The shell tells the user to restart.
    /// </summary>
    public bool RestartRequired { get; private set; }

    /// <summary>Raised each time a revocation makes a restart necessary; the shell shows the prompt.</summary>
    public event EventHandler<RestartRequiredEventArgs>? RestartRequested;

    /// <summary>True when every component a revocation disabled has been saved to the install state.</summary>
    public bool RevocationsPersisted => _unsavedDisabled.Count == 0;

    /// <summary>Why the disabled set is not saved, or null. While set, <see cref="Install"/> and <see cref="Upgrade"/> refuse every package.</summary>
    public ComponentRefusal? RevocationPersistenceFailure { get; private set; }

    /// <summary>What revocation did to installed components so far, oldest first.</summary>
    public IReadOnlyList<RevocationOutcome> RevocationOutcomes => _outcomes;

    /// <summary>Installs a package from its directory at <paramref name="now"/> (UTC seconds).</summary>
    /// <param name="directory">The package root.</param>
    /// <param name="now">The instant to verify at.</param>
    /// <param name="options">Whether a downgrade is allowed, and by whom.</param>
    public ComponentLoadResult Install(string directory, long now, InstallOptions? options = null)
    {
        var effective = options ?? InstallOptions.Default;
        if (BlockedByUnsavedRevocation() is { } blocked)
        {
            return blocked;
        }

        if (!_stateStore.TryLoad(out var state, out var problem))
        {
            return Refuse(ComponentRefusalCodes.StateUnreadable, problem);
        }

        var result = _loader.Load(directory, now, verified => Admit(state, verified, effective));
        if (!result.IsLoaded || result.Component is null)
        {
            return result;
        }

        var saved = Record(state, result.Component, effective, now);
        if (saved is not null)
        {
            // The downgrade rule cannot be enforced next time if this install is not remembered.
            result.Component.Dispose();
            Catalogue.Remove(result.Component.ComponentId, out _);
            return ComponentLoadResult.Refuse(saved);
        }

        return result;
    }

    /// <summary>
    /// Replaces the installed version of a component with the package in <paramref name="directory"/>.
    /// The new package is verified and loaded before the old one is torn down, so a bad upgrade leaves
    /// the working version running. An older version than the highest ever installed is a downgrade
    /// and is refused unless <paramref name="options"/> allows it.
    /// </summary>
    /// <param name="directory">The new package root.</param>
    /// <param name="now">The instant to verify at.</param>
    /// <param name="options">Whether a downgrade is allowed, and by whom.</param>
    public ComponentLoadResult Upgrade(string directory, long now, InstallOptions? options = null)
    {
        var effective = options ?? InstallOptions.Default;
        if (BlockedByUnsavedRevocation() is { } blocked)
        {
            return blocked;
        }

        if (!_stateStore.TryLoad(out var state, out var problem))
        {
            return Refuse(ComponentRefusalCodes.StateUnreadable, problem);
        }

        // A staging catalogue: the load must not touch the installed entry until it has succeeded.
        var staging = new ComponentCatalogue();
        var stagingLoader = new ComponentLoader(_trust, staging, _log, _hostVersion);
        var staged = stagingLoader.Load(directory, now, verified => Admit(state, verified, effective));
        if (!staged.IsLoaded || staged.Component is null)
        {
            return staged;
        }

        var incoming = staged.Component;
        staging.Remove(incoming.ComponentId, out _);

        var saved = Record(state, incoming, effective, now);
        if (saved is not null)
        {
            incoming.Dispose();
            return ComponentLoadResult.Refuse(saved);
        }

        if (Catalogue.TryResolve(incoming.ComponentId, out var installed) && installed is not null)
        {
            Uninstall(incoming.ComponentId);
        }

        var registration = Catalogue.Add(incoming);
        if (registration is not null)
        {
            incoming.Dispose();
            return ComponentLoadResult.Refuse(registration);
        }

        _log.Loaded(incoming);
        return ComponentLoadResult.Loaded(incoming);
    }

    /// <summary>
    /// Checks everything installed against the revocation list in force and disables what it names:
    /// a component whose publisher, signing key or package digest is revoked creates no new instance,
    /// its live instances are disposed, and its context is unloaded where the runtime allows. Where
    /// it cannot be unloaded, <see cref="RestartRequired"/> is set and new instances stay refused.
    /// Runs by itself whenever the trust store accepts a revocation list; the disabled components are
    /// persisted so a restart does not bring them back. This stops further use; it cannot undo what
    /// the code already did in this process.
    /// </summary>
    /// <returns>One outcome per component disabled by this check.</returns>
    public IReadOnlyList<RevocationOutcome> RecheckInstalled()
    {
        var revocations = _trust.Revocations;
        var outcomes = new List<RevocationOutcome>();
        if (revocations is null)
        {
            return outcomes;
        }

        foreach (var component in Catalogue.Components.ToArray())
        {
            var refusal = revocations.CheckPackageFacts(
                component.Verified.PublisherId,
                component.Verified.KeyId,
                component.Verified.SignedBytesSha256);
            if (refusal is null)
            {
                continue;
            }

            Catalogue.Remove(component.ComponentId, out _);
            var version = component.Descriptor.Version.ToString();

            // Code that created an instance has run in this process; unloading proves the context
            // is collectible, not that a thread or timer it started has stopped.
            var hadRun = component.Instances.Count > 0;
            var unloaded = component.Disable(refusal);
            if (unloaded)
            {
                _log.Unloaded(component.ComponentId, version);
            }

            var persistence = Persist(new DisabledComponent(
                component.ComponentId,
                version,
                component.Verified.SignedBytesSha256,
                refusal.Code));
            var restart = !unloaded || hadRun || persistence is not null;
            var outcome = new RevocationOutcome(component.ComponentId, version, refusal, unloaded, restart)
            {
                PersistenceFailure = persistence,
            };
            outcomes.Add(outcome);
            _outcomes.Add(outcome);
            _log.Revoked(outcome);
            if (restart)
            {
                RequireRestart(component.ComponentId);
            }
        }

        return outcomes;
    }

    private ComponentRefusal? Admit(ComponentInstallState state, VerifiedPackage verified, InstallOptions options)
    {
        var id = verified.Manifest.ComponentId;
        var version = verified.Manifest.Version;

        // A package a revocation disabled stays disabled across a restart, even before the registry
        // has re-supplied the list.
        if (state.Disabled.Any(d => string.Equals(d.Digest, verified.SignedBytesSha256, StringComparison.Ordinal)))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.PackageRevoked,
                "'" + id + "' " + version.ToString() + " was disabled by a revocation earlier on this host and stays disabled.");
        }

        if (state.HighestGeneration.TryGetValue(id, out var generation) && _trust.HeldRevocationGeneration < generation)
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.GenerationRegressed,
                "'" + id + "' was installed under revocation generation "
                + generation.ToString(System.Globalization.CultureInfo.InvariantCulture)
                + " but the store now holds an older one: the list was rolled back, or has not been loaded yet.");
        }

        if (state.TryGetHighest(id, out var highest) && version.CompareTo(highest) < 0)
        {
            if (!options.AllowDowngrade || string.IsNullOrWhiteSpace(options.Actor))
            {
                return new ComponentRefusal(
                    ComponentRefusalCodes.DowngradeRefused,
                    "'" + id + "' " + version.ToString() + " is older than " + highest.ToString()
                    + ", which this host installed before. Revoke the newer version to roll back, "
                    + "or allow the downgrade explicitly with the name of who allows it.");
            }
        }

        return null;
    }

    // Remembers an install: the highest version and generation, and the log line. Returns a refusal
    // when the state cannot be written.
    private ComponentRefusal? Record(ComponentInstallState state, LoadedComponent component, InstallOptions options, long now)
    {
        var id = component.ComponentId;
        var version = component.Descriptor.Version;
        var hadHighest = state.TryGetHighest(id, out var previous);
        var downgrade = hadHighest && version.CompareTo(previous) < 0;
        if (!hadHighest || version.CompareTo(previous) > 0)
        {
            state.HighestVersion[id] = version.ToString();
        }

        var generation = component.Verified.RevocationGeneration;
        if (!state.HighestGeneration.TryGetValue(id, out var heldGeneration) || generation > heldGeneration)
        {
            state.HighestGeneration[id] = generation;
        }

        var entry = new InstallLogEntry(
            id,
            version.ToString(),
            hadHighest ? previous.ToString() : null,
            downgrade && options.AllowDowngrade,
            downgrade && options.AllowDowngrade ? options.ActorText : string.Empty,
            now);
        state.Log.Add(entry);
        if (!_stateStore.TrySave(state, out var problem))
        {
            return new ComponentRefusal(ComponentRefusalCodes.StateUnwritable, problem ?? "the install state could not be written.");
        }

        _log.InstallRecorded(entry);
        return null;
    }

    /// <summary>
    /// Saves the disabled components that could not be saved earlier. Call it after the store has
    /// been repaired; <see cref="Install"/> and <see cref="Upgrade"/> stay refused until it succeeds.
    /// </summary>
    public RevocationPersistResult RetryPersistRevocations()
    {
        if (_unsavedDisabled.Count == 0)
        {
            return new RevocationPersistResult(true, null);
        }

        var failure = Flush();
        return new RevocationPersistResult(failure is null, failure);
    }

    private ComponentLoadResult? BlockedByUnsavedRevocation()
    {
        if (_unsavedDisabled.Count == 0)
        {
            return null;
        }

        return Refuse(
            ComponentRefusalCodes.RevocationNotPersisted,
            "a revocation disabled a component but the install state could not be saved: no package is installed until it is saved (retry the save).");
    }

    private ComponentRefusal? Persist(DisabledComponent entry)
    {
        _unsavedDisabled.Add(entry);
        return Flush();
    }

    // Writes every unsaved disabled component into the install state. A store that fails or throws
    // leaves them unsaved, and that is reported: the refusal is kept, logged and returned.
    private ComponentRefusal? Flush()
    {
        var saved = false;
        try
        {
            if (_stateStore.TryLoad(out var state, out _))
            {
                foreach (var entry in _unsavedDisabled)
                {
                    if (!state.Disabled.Any(d => string.Equals(d.Digest, entry.Digest, StringComparison.Ordinal)))
                    {
                        state.Disabled.Add(entry);
                    }
                }

                saved = _stateStore.TrySave(state, out _);
            }
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            // A store that throws is a store that failed: reported below, never propagated and
            // never copied into the message (its text may hold a path).
            saved = false;
        }

        if (saved)
        {
            _unsavedDisabled.Clear();
            RevocationPersistenceFailure = null;
            return null;
        }

        var failure = new ComponentRefusal(
            ComponentRefusalCodes.RevocationNotPersisted,
            "a revocation disabled a component but the install state could not be read or saved: a restart could bring it back, so no package is installed until the state is saved.");
        RevocationPersistenceFailure = failure;
        _log.Refused(failure);
        return failure;
    }

    private void RequireRestart(string componentId)
    {
        RestartRequired = true;
        RestartRequested?.Invoke(this, new RestartRequiredEventArgs(ComponentRefusalCodes.RestartRequired, componentId));
    }

    private ComponentLoadResult Refuse(string code, string? message)
    {
        var refusal = new ComponentRefusal(code, message ?? "the install state could not be read.");
        _log.Refused(refusal);
        return ComponentLoadResult.Refuse(refusal);
    }

    /// <summary>Uninstalls a component: disposes its instances, clears its registration, unloads its context.</summary>
    /// <param name="componentId">The component id.</param>
    public ComponentUninstallResult Uninstall(string componentId)
    {
        if (!Catalogue.Remove(componentId, out var installed) || installed is null)
        {
            return ComponentUninstallResult.Refuse(new ComponentRefusal(
                ComponentRefusalCodes.ComponentUnknown,
                "'" + componentId + "' is not installed."));
        }

        var instances = installed.Instances.Count;
        var version = installed.Descriptor.Version.ToString();
        installed.Dispose();
        _log.Unloaded(componentId, version);
        return ComponentUninstallResult.Done(instances);
    }

    /// <summary>
    /// Proves the teardown of a component's context: uninstalls it and returns a weak reference to the
    /// unloaded context, so a caller can force a collection and see whether the code went away. This is
    /// the check the uninstall test makes; a shell may also run it at start-up in debug builds.
    /// </summary>
    /// <param name="componentId">The component id.</param>
    /// <param name="result">The uninstall outcome.</param>
    public WeakReference UninstallAndTrack(string componentId, out ComponentUninstallResult result)
    {
        if (!Catalogue.TryResolve(componentId, out var installed) || installed is null)
        {
            result = ComponentUninstallResult.Refuse(new ComponentRefusal(
                ComponentRefusalCodes.ComponentUnknown,
                "'" + componentId + "' is not installed."));
            return new WeakReference(null);
        }

        var weak = new WeakReference(installed.Context);
        result = Uninstall(componentId);
        return weak;
    }

    /// <summary>Disposes every installed component: what a host does when the shell closes.</summary>
    public void UninstallAll()
    {
        foreach (var component in Catalogue.Components.ToArray())
        {
            Uninstall(component.ComponentId);
        }
    }

    /// <summary>The installed versions, as text, for a diagnostics line.</summary>
    public string DescribeInstalled() =>
        Catalogue.Count == 0
            ? "no components installed"
            : string.Join(", ", Catalogue.Components.Select(static c =>
                c.ComponentId + "@" + c.Descriptor.Version.ToString()));

    /// <summary>Keeps the JIT from extending a lifetime the test wants to end (used by the unload test).</summary>
    /// <param name="value">The value to keep alive until here.</param>
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void KeepAlive(object? value) => GC.KeepAlive(value);
}
