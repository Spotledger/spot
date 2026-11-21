namespace Spotledger.Client.Grid;

/// <summary>Shared numeric defaults for the grid. Values here are geometry constants
/// only — every colour and density size comes from Theme tokens at render time.</summary>
public static class GridDefaults
{
    /// <summary>The tightest a column may be squeezed.</summary>
    public const double MinimumColumnWidth = 24;

    /// <summary>Edge band where the header cursor becomes a resize grip.</summary>
    public const double HeaderResizeGripWidth = 5;
}