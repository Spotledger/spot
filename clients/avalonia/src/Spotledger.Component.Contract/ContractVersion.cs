using System.Globalization;
using System.Text.RegularExpressions;

namespace Spotledger.Component.Contract;

/// <summary>
/// The version of the component contract itself. Versioning follows the WIT world rule the
/// blueprint names: a <b>major</b> bump breaks source and binary compatibility (a member is
/// removed, a signature changes, a semantic changes); a <b>minor</b> bump is additive only
/// (a new member with a default, a new optional descriptor field, a new event the host may
/// ignore). A component built against one minor therefore works on every later minor of the
/// same major, and on no other major.
/// </summary>
public readonly partial record struct ContractVersion : IComparable<ContractVersion>
{
    /// <summary>Creates a contract version.</summary>
    public ContractVersion(int major, int minor)
    {
        if (major < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(major), "A major version is never negative.");
        }

        if (minor < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(minor), "A minor version is never negative.");
        }

        Major = major;
        Minor = minor;
    }

    /// <summary>The breaking-change counter.</summary>
    public int Major { get; }

    /// <summary>The additive-change counter.</summary>
    public int Minor { get; }

    /// <summary>Parses the <c>major.minor</c> form used by descriptors, refusing anything else.</summary>
    public static ContractVersion Parse(string text)
    {
        if (!TryParse(text, out var version))
        {
            throw new FormatException("A contract version is written 'major.minor', for example '1.0'; got '" + text + "'.");
        }

        return version;
    }

    /// <summary>Parses the <c>major.minor</c> form, returning false instead of throwing.</summary>
    public static bool TryParse(string? text, out ContractVersion version)
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

        var major = int.Parse(match.Groups["major"].Value, CultureInfo.InvariantCulture);
        var minor = int.Parse(match.Groups["minor"].Value, CultureInfo.InvariantCulture);
        version = new ContractVersion(major, minor);
        return true;
    }

    /// <inheritdoc />
    public int CompareTo(ContractVersion other) =>
        Major != other.Major ? Major.CompareTo(other.Major) : Minor.CompareTo(other.Minor);

    /// <summary>True when the two versions share a major, so the newer one only adds.</summary>
    public bool IsSameMajorAs(ContractVersion other) => Major == other.Major;

    /// <summary>True when <paramref name="other"/> is a strictly later version.</summary>
    public bool IsOlderThan(ContractVersion other) => CompareTo(other) < 0;

    /// <inheritdoc />
    public override string ToString() => Major.ToString(CultureInfo.InvariantCulture) + "." + Minor.ToString(CultureInfo.InvariantCulture);

    /// <summary>True when <paramref name="left"/> is an earlier version than <paramref name="right"/>.</summary>
    public static bool operator <(ContractVersion left, ContractVersion right) => left.CompareTo(right) < 0;

    /// <summary>True when <paramref name="left"/> is a later version than <paramref name="right"/>.</summary>
    public static bool operator >(ContractVersion left, ContractVersion right) => left.CompareTo(right) > 0;

    /// <summary>True when <paramref name="left"/> is an earlier or equal version.</summary>
    public static bool operator <=(ContractVersion left, ContractVersion right) => left.CompareTo(right) <= 0;

    /// <summary>True when <paramref name="left"/> is a later or equal version.</summary>
    public static bool operator >=(ContractVersion left, ContractVersion right) => left.CompareTo(right) >= 0;

    [GeneratedRegex("^(?<major>0|[1-9][0-9]*)\\.(?<minor>0|[1-9][0-9]*)$", RegexOptions.CultureInvariant)]
    private static partial Regex VersionPattern();
}

/// <summary>
/// The closed range of contract versions a component declares it targets, written
/// <c>&gt;=1.0 &lt;2.0</c>. A host refuses a component whose range does not contain the host's
/// own contract version, and refuses a range that crosses a major (that range would claim
/// compatibility with a breaking change).
/// </summary>
public sealed record ContractVersionRange
{
    /// <summary>Creates a range; <paramref name="maximumExclusive"/> is the first version not covered.</summary>
    public ContractVersionRange(ContractVersion minimum, ContractVersion maximumExclusive)
    {
        if (minimum >= maximumExclusive)
        {
            throw new ArgumentException("A contract range's minimum must be lower than its maximum.", nameof(minimum));
        }

        if (maximumExclusive.Major != minimum.Major + 1)
        {
            throw new ArgumentException(
                "A contract range covers exactly one major version, so its maximum is the first version of the next major (for example >=1.0 <2.0).",
                nameof(maximumExclusive));
        }

        Minimum = minimum;
        MaximumExclusive = maximumExclusive;
    }

    /// <summary>The lowest contract version the component supports (inclusive).</summary>
    public ContractVersion Minimum { get; }

    /// <summary>The first contract version the component does not support (exclusive).</summary>
    public ContractVersion MaximumExclusive { get; }

    /// <summary>The range a component built against exactly one contract version declares: the whole major.</summary>
    public static ContractVersionRange ForMajor(ContractVersion version) =>
        new(new ContractVersion(version.Major, 0), new ContractVersion(version.Major + 1, 0));

    /// <summary>The same range expressed for the current contract version.</summary>
    public static ContractVersionRange CurrentMajor => ForMajor(ComponentContract.Version);

    /// <summary>Parses <c>&gt;=1.0 &lt;2.0</c> (spaces optional); refuses a cross-major range.</summary>
    public static ContractVersionRange Parse(string text)
    {
        if (!TryParse(text, out var range))
        {
            throw new FormatException("A contract range is written '>=1.0 <2.0'; got '" + text + "'.");
        }

        return range;
    }

    /// <summary>Parses the range form, returning false instead of throwing.</summary>
    public static bool TryParse(string? text, out ContractVersionRange range)
    {
        range = null!;
        if (string.IsNullOrWhiteSpace(text))
        {
            return false;
        }

        var parts = text.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        if (parts.Length != 2 || !parts[0].StartsWith(">=", StringComparison.Ordinal) || !parts[1].StartsWith('<'))
        {
            return false;
        }

        if (!ContractVersion.TryParse(parts[0][2..], out var minimum)
            || !ContractVersion.TryParse(parts[1][1..], out var maximum))
        {
            return false;
        }

        if (minimum >= maximum || maximum.Major != minimum.Major + 1)
        {
            return false;
        }

        range = new ContractVersionRange(minimum, maximum);
        return true;
    }

    /// <summary>True when the host's version falls inside the range.</summary>
    public bool Contains(ContractVersion version) => version >= Minimum && version < MaximumExclusive;

    /// <inheritdoc />
    public override string ToString() =>
        ">=" + Minimum.ToString() + " <" + MaximumExclusive.ToString();
}

/// <summary>
/// The contract version this build of the host and kit speak, plus the compatibility rules in
/// executable form. Descriptors declare a range; <see cref="ComponentRegistry"/> refuses any
/// range that does not contain the host's version.
/// </summary>
public static class ComponentContract
{
    /// <summary>The contract version this build implements.</summary>
    public static ContractVersion Version { get; } = new(1, 0);

    /// <summary>The range every host of this build accepts.</summary>
    public static ContractVersionRange SupportedRange { get; } = ContractVersionRange.ForMajor(Version);

    /// <summary>True when a descriptor's declared range can run on this host.</summary>
    public static bool IsCompatible(ContractVersionRange declared) => declared.Contains(Version);

    /// <summary>
    /// The compatibility verdict for a declared range, as a stable code and a message a
    /// reviewer can act on: <c>component.contract.too_new</c> when the component requires a
    /// later contract than this host speaks, <c>component.contract.unsupported</c> otherwise.
    /// </summary>
    public static bool TryAccept(ContractVersionRange declared, out string? refusalCode, out string? refusal)
    {
        ArgumentNullException.ThrowIfNull(declared);
        if (declared.Contains(Version))
        {
            refusalCode = null;
            refusal = null;
            return true;
        }

        refusalCode = declared.Minimum > Version ? "component.contract.too_new" : "component.contract.unsupported";
        refusal = "Component targets contract " + declared.ToString()
            + " but this host speaks " + Version.ToString() + ".";
        return false;
    }
}
