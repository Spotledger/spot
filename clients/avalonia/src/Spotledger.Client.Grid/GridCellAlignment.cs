namespace Spotledger.Client.Grid;

/// <summary>Horizontal cell text placement. Start/End follow the writing direction,
/// so a cell aligned Start sits on the left when the grid flows LTR and on the right
/// when it flows RTL; Center never moves.</summary>
public enum GridCellAlignment
{
    /// <summary>Leading edge of the cell (left in LTR, right in RTL). The default.</summary>
    Start,

    /// <summary>Horizontally centred inside the cell.</summary>
    Center,

    /// <summary>Trailing edge of the cell (right in LTR, left in RTL).</summary>
    End,
}