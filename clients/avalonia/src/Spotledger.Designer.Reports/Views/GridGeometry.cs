namespace Spotledger.Designer.Reports.Views;

/// <summary>The pixel size of a dashboard grid cell, and the drop arithmetic that follows from it.</summary>
public static class GridGeometry
{
    /// <summary>The width of one cell, in device-independent pixels.</summary>
    public const double CellWidth = 44;

    /// <summary>The height of one cell, in device-independent pixels.</summary>
    public const double CellHeight = 30;

    /// <summary>The place a tile that started at <paramref name="origin"/> lands on when dragged by this many pixels.</summary>
    /// <remarks>The result is only a request: the server refuses a place that overlaps or falls off the grid.</remarks>
    public static Place DragTarget(Place origin, double deltaX, double deltaY)
    {
        ArgumentNullException.ThrowIfNull(origin);
        var columns = (int)Math.Round(deltaX / CellWidth, MidpointRounding.AwayFromZero);
        var rows = (int)Math.Round(deltaY / CellHeight, MidpointRounding.AwayFromZero);
        return origin with { Row = origin.Row + rows, Column = origin.Column + columns };
    }

    /// <summary>
    /// The place a tile of this size starts at when it is dropped with its top-left corner on the
    /// cell under a point. A point left of or above the grid gives a row or column below one, which
    /// the server refuses as off the grid; that is what makes a drop outside change nothing.
    /// </summary>
    public static Place CellAt(double x, double y, int width, int height) =>
        new((int)Math.Floor(y / CellHeight) + 1, (int)Math.Floor(x / CellWidth) + 1, width, height);

    /// <summary>Whether a point is inside a grid of this many cells.</summary>
    public static bool Within(double x, double y, int columns, int rows) =>
        x >= 0 && y >= 0 && x < columns * CellWidth && y < rows * CellHeight;
}
