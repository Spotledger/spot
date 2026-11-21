namespace Spotledger.Client.Shell;

/// <summary>The four availability states a command can have (blueprint corrections §4).</summary>
public enum AvailabilityState
{
    /// <summary>The command runs now.</summary>
    Available,

    /// <summary>State makes it impossible right now (e.g. no tab is open).</summary>
    Blocked,

    /// <summary>The command needs context the trigger did not carry (e.g. a record).</summary>
    NeedsContext,

    /// <summary>The signed-in user lacks the capability.</summary>
    Restricted,
}

/// <summary>
/// A command's availability with a human-readable reason. For now the shell answers from
/// local state; the seam that lets the server answer (ADR-055) replaces the provider,
/// not the consumers.
/// </summary>
public sealed record CommandAvailability(AvailabilityState State, string? Reason = null, string? ResolutionCommandId = null)
{
    public static readonly CommandAvailability Available = new(AvailabilityState.Available);

    public bool IsAvailable => State == AvailabilityState.Available;

    public static CommandAvailability Blocked(string reason) => new(AvailabilityState.Blocked, reason);

    public static CommandAvailability NeedsContext(string reason) => new(AvailabilityState.NeedsContext, reason);

    public static CommandAvailability Restricted(string reason) => new(AvailabilityState.Restricted, reason);
}
