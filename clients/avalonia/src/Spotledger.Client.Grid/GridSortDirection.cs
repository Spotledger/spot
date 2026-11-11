namespace Spotledger.Client.Grid;

/// <summary>Sort state for one column: the header click cycle is
/// none → ascending → descending → none.</summary>
public enum GridSortDirection
{
    /// <summary>No sort applied to the column.</summary>
    None,

    /// <summary>Ascending; the header draws the up indicator.</summary>
    Ascending,

    /// <summary>Descending; the header draws the down indicator.</summary>
    Descending,
}