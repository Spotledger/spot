namespace Spotledger.Client.Grid;

/// <summary>
/// The source's statement about its row count: known exactly, estimated, or unknown.
/// The grid uses the value for vertical bounds and scrollbar proportions; when the
/// kind is <see cref="GridRowCountKind.Unknown"/> the grid derives the visible total
/// from the furthest window it has received so far.
/// </summary>
public readonly struct GridRowCount : IEquatable<GridRowCount>
{
    /// <summary>Creates a count statement. Prefer <see cref="Known"/>, <see cref="Estimated"/>
    /// and <see cref="Unknown"/>; the value must not be negative (an empty source is Known(0)).</summary>
    public GridRowCount(GridRowCountKind kind, int? value)
    {
        if (value is { } v && v < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(value), "A row count cannot be negative.");
        }

        Kind = kind;
        Value = value;
    }

    /// <summary>Exactly <paramref name="value"/> rows.</summary>
    public static GridRowCount Known(int value) => new(GridRowCountKind.Known, value);

    /// <summary>Approximately <paramref name="value"/> rows; the estimate may drift.</summary>
    public static GridRowCount Estimated(int value) => new(GridRowCountKind.Estimated, value);

    /// <summary>The source cannot state a count; the grid learns it from windows.</summary>
    public static GridRowCount Unknown() => new(GridRowCountKind.Unknown, null);

    /// <summary>How much the value can be trusted.</summary>
    public GridRowCountKind Kind { get; }

    /// <summary>The stated count, or <see langword="null"/> when the kind is Unknown.</summary>
    public int? Value { get; }

    /// <summary>True when the source can state any number at all.</summary>
    public bool HasValue => Value is not null;

    /// <summary>Equality: same kind and same value.</summary>
    public static bool operator ==(GridRowCount left, GridRowCount right) => left.Equals(right);

    /// <summary>Inequality, the negation of <see cref="Equals(GridRowCount)"/>.</summary>
    public static bool operator !=(GridRowCount left, GridRowCount right) => !left.Equals(right);

    /// <summary>Equality: same kind and same value.</summary>
    public bool Equals(GridRowCount other) => Kind == other.Kind && Value == other.Value;

    /// <inheritdoc />
    public override bool Equals(object? obj) => obj is GridRowCount other && Equals(other);

    /// <inheritdoc />
    public override int GetHashCode() => HashCode.Combine(Kind, Value);
}