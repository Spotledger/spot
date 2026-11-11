using System.Globalization;
using System.Text.RegularExpressions;

namespace Spotledger.Component.Contract;

/// <summary>
/// The version of a component package (its own SemVer, not the contract's): a patch fixes
/// behaviour, a minor adds a prop with a default, a major changes a prop's meaning.
/// </summary>
public readonly partial record struct ComponentVersion : IComparable<ComponentVersion>
{
    /// <summary>Creates a component version.</summary>
    public ComponentVersion(int major, int minor, int patch = 0)
    {
        if (major < 0 || minor < 0 || patch < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(major), "Version parts are never negative.");
        }

        Major = major;
        Minor = minor;
        Patch = patch;
    }

    /// <summary>The major part.</summary>
    public int Major { get; }

    /// <summary>The minor part.</summary>
    public int Minor { get; }

    /// <summary>The patch part.</summary>
    public int Patch { get; }

    /// <summary>Parses <c>major.minor.patch</c>; <c>major.minor</c> is accepted with patch 0.</summary>
    public static bool TryParse(string? text, out ComponentVersion version)
    {
        version = default;
        if (string.IsNullOrWhiteSpace(text))
        {
            return false;
        }

        var match = VersionPattern().Match(text.Trim());
        if (!match.Success)
        {
            return false;
        }

        version = new ComponentVersion(
            int.Parse(match.Groups["major"].Value, CultureInfo.InvariantCulture),
            int.Parse(match.Groups["minor"].Value, CultureInfo.InvariantCulture),
            match.Groups["patch"].Success ? int.Parse(match.Groups["patch"].Value, CultureInfo.InvariantCulture) : 0);
        return true;
    }

    /// <summary>Parses the version, throwing on anything else.</summary>
    public static ComponentVersion Parse(string text) =>
        TryParse(text, out var version)
            ? version
            : throw new FormatException("A component version is written 'major.minor.patch'; got '" + text + "'.");

    /// <inheritdoc />
    public int CompareTo(ComponentVersion other)
    {
        var major = Major.CompareTo(other.Major);
        if (major != 0)
        {
            return major;
        }

        var minor = Minor.CompareTo(other.Minor);
        return minor != 0 ? minor : Patch.CompareTo(other.Patch);
    }

    /// <summary>True when <paramref name="left"/> is an earlier version.</summary>
    public static bool operator <(ComponentVersion left, ComponentVersion right) => left.CompareTo(right) < 0;

    /// <summary>True when <paramref name="left"/> is a later version.</summary>
    public static bool operator >(ComponentVersion left, ComponentVersion right) => left.CompareTo(right) > 0;

    /// <summary>True when <paramref name="left"/> is earlier or equal.</summary>
    public static bool operator <=(ComponentVersion left, ComponentVersion right) => left.CompareTo(right) <= 0;

    /// <summary>True when <paramref name="left"/> is later or equal.</summary>
    public static bool operator >=(ComponentVersion left, ComponentVersion right) => left.CompareTo(right) >= 0;

    /// <inheritdoc />
    public override string ToString() =>
        Major.ToString(CultureInfo.InvariantCulture) + "."
        + Minor.ToString(CultureInfo.InvariantCulture) + "."
        + Patch.ToString(CultureInfo.InvariantCulture);

    [GeneratedRegex("^(?<major>0|[1-9][0-9]*)\\.(?<minor>0|[1-9][0-9]*)(\\.(?<patch>0|[1-9][0-9]*))?$", RegexOptions.CultureInvariant)]
    private static partial Regex VersionPattern();
}
