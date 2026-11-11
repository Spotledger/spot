using System.Text.Json;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>One line of the install log: who installed what, and whether a downgrade was allowed.</summary>
/// <param name="ComponentId">The component.</param>
/// <param name="Version">The version installed.</param>
/// <param name="PreviousHighest">The highest version this host had installed before, or null for the first install.</param>
/// <param name="DowngradeAllowed">True when the caller passed <see cref="InstallOptions.AllowDowngrade"/> and the version was lower.</param>
/// <param name="Actor">Who allowed the downgrade (empty when none was needed).</param>
/// <param name="At">The instant (UTC seconds) the caller passed in; the host reads no clock.</param>
public sealed record InstallLogEntry(
    string ComponentId,
    string Version,
    string? PreviousHighest,
    bool DowngradeAllowed,
    string Actor,
    long At);

/// <summary>A component a revocation list disabled, remembered so a restart does not re-enable it.</summary>
/// <param name="ComponentId">The component.</param>
/// <param name="Version">Its version.</param>
/// <param name="Digest">The digest of its signed manifest, lower-case hex.</param>
/// <param name="Code">The refusal code that applied.</param>
public sealed record DisabledComponent(string ComponentId, string Version, string Digest, string Code);

/// <summary>
/// What the host remembers across restarts about installs: per component id the highest version it
/// ever installed and the highest store revocation generation it installed under, the components a
/// revocation list disabled, and the install log. It holds no secret.
/// </summary>
public sealed class ComponentInstallState
{
    /// <summary>The highest installed version per component id, as <c>major.minor.patch</c>.</summary>
    public Dictionary<string, string> HighestVersion { get; set; } = new(StringComparer.Ordinal);

    /// <summary>The highest store revocation generation per component id at install time.</summary>
    public Dictionary<string, ulong> HighestGeneration { get; set; } = new(StringComparer.Ordinal);

    /// <summary>The components revocation disabled.</summary>
    public List<DisabledComponent> Disabled { get; set; } = new();

    /// <summary>The install log, oldest first.</summary>
    public List<InstallLogEntry> Log { get; set; } = new();

    /// <summary>The highest version recorded for a component, when there is one.</summary>
    /// <param name="componentId">The component.</param>
    /// <param name="version">The version.</param>
    public bool TryGetHighest(string componentId, out ComponentVersion version)
    {
        version = default;
        return HighestVersion.TryGetValue(componentId, out var text) && ComponentVersion.TryParse(text, out version);
    }
}

/// <summary>Where the install state lives. A failure is reported, never thrown: the host then refuses installs.</summary>
public interface IComponentStateStore
{
    /// <summary>Reads the state; a store that holds none yields an empty state.</summary>
    /// <param name="state">The state.</param>
    /// <param name="problem">Plain text on failure.</param>
    bool TryLoad(out ComponentInstallState state, out string? problem);

    /// <summary>Writes the state.</summary>
    /// <param name="state">The state.</param>
    /// <param name="problem">Plain text on failure.</param>
    bool TrySave(ComponentInstallState state, out string? problem);
}

/// <summary>State kept in memory only: for a host that does not persist, and for tests.</summary>
public sealed class InMemoryComponentStateStore : IComponentStateStore
{
    private string _json = Serialise(new ComponentInstallState());

    /// <inheritdoc />
    public bool TryLoad(out ComponentInstallState state, out string? problem)
    {
        state = JsonSerializer.Deserialize<ComponentInstallState>(_json) ?? new ComponentInstallState();
        problem = null;
        return true;
    }

    /// <inheritdoc />
    public bool TrySave(ComponentInstallState state, out string? problem)
    {
        ArgumentNullException.ThrowIfNull(state);
        _json = Serialise(state);
        problem = null;
        return true;
    }

    internal static string Serialise(ComponentInstallState state) => JsonSerializer.Serialize(state);
}

/// <summary>State kept in one JSON file, written through a temporary file so a crash never leaves half a file.</summary>
public sealed class FileComponentStateStore : IComponentStateStore
{
    private readonly string _path;

    /// <summary>Creates a store over a file path.</summary>
    /// <param name="path">The state file; created on the first save.</param>
    public FileComponentStateStore(string path)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        _path = path;
    }

    /// <inheritdoc />
    public bool TryLoad(out ComponentInstallState state, out string? problem)
    {
        state = new ComponentInstallState();
        problem = null;
        try
        {
            if (!File.Exists(_path))
            {
                return true;
            }

            state = JsonSerializer.Deserialize<ComponentInstallState>(File.ReadAllText(_path)) ?? new ComponentInstallState();
            return true;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException or NotSupportedException)
        {
            state = new ComponentInstallState();
            problem = "the install state file could not be read; installs are refused until it is repaired or removed.";
            return false;
        }
    }

    /// <inheritdoc />
    public bool TrySave(ComponentInstallState state, out string? problem)
    {
        ArgumentNullException.ThrowIfNull(state);
        problem = null;
        try
        {
            var temporary = _path + ".tmp";
            File.WriteAllText(temporary, InMemoryComponentStateStore.Serialise(state));
            File.Move(temporary, _path, overwrite: true);
            return true;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or NotSupportedException)
        {
            problem = "the install state file could not be written.";
            return false;
        }
    }
}

/// <summary>Options for one install or upgrade.</summary>
/// <param name="AllowDowngrade">True to accept a version lower than the highest ever installed. Revoking the newer version is the normal way back; this is the explicit, recorded exception.</param>
/// <param name="Actor">Who allows it; required when <paramref name="AllowDowngrade"/> is set, and written to the install log.</param>
public sealed record InstallOptions(bool AllowDowngrade = false, string? Actor = null)
{
    /// <summary>No downgrade allowed.</summary>
    public static InstallOptions Default { get; } = new();

    internal string ActorText => Actor ?? string.Empty;

}

/// <summary>What a revocation list did to one installed component.</summary>
/// <param name="ComponentId">The component.</param>
/// <param name="Version">Its version.</param>
/// <param name="Refusal">The revocation that applies (publisher, key or package), as accessible text.</param>
/// <param name="Unloaded">True when the code was unloaded.</param>
/// <param name="RestartRequired">True when the code could not be unloaded and the process must restart to be rid of it; new instances are refused meanwhile.</param>
public sealed record RevocationOutcome(
    string ComponentId,
    string Version,
    ComponentRefusal Refusal,
    bool Unloaded,
    bool RestartRequired)
{
    /// <summary>
    /// Null when the disabled component was saved to the install state. Otherwise the refusal
    /// (<see cref="ComponentRefusalCodes.RevocationNotPersisted"/>): the save failed, so a restart
    /// could bring the component back, installs are refused and the shell must say so.
    /// </summary>
    public ComponentRefusal? PersistenceFailure { get; init; }
}

/// <summary>Whether the disabled set was saved to the install state, and the refusal when it was not.</summary>
/// <param name="Persisted">True when nothing is waiting to be saved.</param>
/// <param name="Refusal">The <see cref="ComponentRefusalCodes.RevocationNotPersisted"/> refusal when the save failed; null otherwise.</param>
public sealed record RevocationPersistResult(bool Persisted, ComponentRefusal? Refusal);

/// <summary>Raised when the shell should tell the user to restart the application.</summary>
public sealed class RestartRequiredEventArgs : EventArgs
{
    /// <summary>Creates the arguments.</summary>
    /// <param name="code">The stable code, <see cref="ComponentRefusalCodes.RestartRequired"/>.</param>
    /// <param name="componentId">The revoked component that makes a restart necessary.</param>
    public RestartRequiredEventArgs(string code, string componentId)
    {
        Code = code ?? throw new ArgumentNullException(nameof(code));
        ComponentId = componentId ?? throw new ArgumentNullException(nameof(componentId));
    }

    /// <summary>The stable code, <see cref="ComponentRefusalCodes.RestartRequired"/>.</summary>
    public string Code { get; }

    /// <summary>The revoked component that makes a restart necessary.</summary>
    public string ComponentId { get; }
}
