namespace Spotledger.Client.Controls;

/// <summary>
/// Digit grouping and decimal separators for the numeric inputs. Deliberately not
/// <see cref="System.Globalization.NumberFormatInfo"/>: the value comes from the document's
/// locale settings, never from the OS culture (ADR-12 money rules; WP-DOC-02 owns rounding).
/// </summary>
/// <param name="GroupSeparator">Thousands grouping separator, e.g. ",".</param>
/// <param name="DecimalSeparator">Decimal point separator, e.g. ".".</param>
public readonly record struct SpotNumberFormat(string GroupSeparator, string DecimalSeparator)
{
    /// <summary>The default display format: 1,234.50.</summary>
    public static SpotNumberFormat Default { get; } = new(",", ".");

    /// <summary>A European-style format: 1.234,50.</summary>
    public static SpotNumberFormat European { get; } = new(".", ",");

    /// <summary>No grouping at all (raw digits).</summary>
    public static SpotNumberFormat Plain { get; } = new(string.Empty, ".");
}
