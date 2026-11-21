using System.Globalization;

namespace Spotledger.Client.Grid;

/// <summary>A column width: a fixed pixel value or a star (proportional) weight, like
/// XAML's <c>Absolute</c>/<c>Star</c> grid sizing. Star columns share the viewport width
/// left over after every pixel column; when the minimum widths overrun the viewport the
/// content scrolls horizontally.</summary>
public readonly struct GridColumnWidth : IEquatable<GridColumnWidth>
{
    private GridColumnWidth(double value, bool isStar)
    {
        Value = value;
        IsStar = isStar;
    }

    /// <summary>A fixed width of <paramref name="pixels"/> device-independent pixels.</summary>
    public static GridColumnWidth Pixels(double pixels)
    {
        if (!double.IsFinite(pixels) || pixels <= 0)
        {
            throw new ArgumentOutOfRangeException(nameof(pixels), "A pixel column width must be a positive finite number.");
        }

        return new GridColumnWidth(pixels, isStar: false);
    }

    /// <summary>A star width with <paramref name="weight"/> parts (<c>2*</c> = twice a <c>1*</c>).</summary>
    public static GridColumnWidth Star(double weight = 1)
    {
        if (!double.IsFinite(weight) || weight <= 0)
        {
            throw new ArgumentOutOfRangeException(nameof(weight), "A star width must have a positive finite weight.");
        }

        return new GridColumnWidth(weight, isStar: true);
    }

    /// <summary>The pixel amount or star weight, by <see cref="IsStar"/>.</summary>
    public double Value { get; }

    /// <summary>One record in a width list. True when this is a proportional (star) width.</summary>
    public bool IsStar { get; }

    /// <summary>Equality: same is-star and same value.</summary>
    public static bool operator ==(GridColumnWidth left, GridColumnWidth right) => left.Equals(right);

    /// <summary>Inequality, the negation of <see cref="Equals(GridColumnWidth)"/>.</summary>
    public static bool operator !=(GridColumnWidth left, GridColumnWidth right) => !left.Equals(right);

    /// <inheritdoc />
    public bool Equals(GridColumnWidth other) => IsStar == other.IsStar && Value.Equals(other.Value);

    /// <inheritdoc />
    public override bool Equals(object? obj) => obj is GridColumnWidth other && Equals(other);

    /// <inheritdoc />
    public override int GetHashCode() => HashCode.Combine(IsStar, Value);

    /// <inheritdoc />
    public override string ToString() =>
        IsStar
            ? FormattableString.Invariant($"{Value.ToString("0.##", CultureInfo.InvariantCulture)}*")
            : FormattableString.Invariant($"{Value.ToString("0.##", CultureInfo.InvariantCulture)}px");
}