namespace Spotledger.Client.Controls;

/// <summary>
/// Density modes for the control library. The <see cref="Density"/> attached property
/// switches controls between the Theme's Compact and Comfortable token sets.
/// </summary>
public enum DensityMode
{
    /// <summary>Taller controls with generous padding — the library default.</summary>
    Comfortable,

    /// <summary>Short controls for data-dense forms and grids.</summary>
    Compact
}
