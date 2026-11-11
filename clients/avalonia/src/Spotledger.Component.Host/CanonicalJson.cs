using System.Globalization;
using System.Text;

namespace Spotledger.Component.Host;

/// <summary>
/// One value of the canonical JSON the signing scheme covers.
/// <para>
/// The rules are the ones <c>crates/framework/spot-bundle-signing/src/canonical.rs</c> writes with
/// <c>serde_json</c>: an object's keys are sorted (ordinal, which is byte order for ASCII keys), no
/// whitespace anywhere, integers only (there is no number type that can hold a fraction, so money
/// and quantities cannot be signed by accident), booleans as <c>true</c>/<c>false</c>, and strings
/// escaped exactly as serde_json escapes them — <c>"</c>, <c>\</c>, <c>\b</c>, <c>\f</c>, <c>\n</c>,
/// <c>\r</c>, <c>\t</c>, every other code point below <c>U+0020</c> as <c>\u00xx</c> in lower-case
/// hex, and everything else (including non-ASCII) as raw UTF-8.
/// </para>
/// </summary>
public abstract class CanonicalValue
{
    private protected CanonicalValue()
    {
    }
}

/// <summary>A canonical JSON object: members sorted by key, duplicates refused.</summary>
public sealed class CanonicalObject : CanonicalValue
{
    private readonly SortedDictionary<string, CanonicalValue> _members;

    private CanonicalObject(SortedDictionary<string, CanonicalValue> members)
    {
        _members = members;
    }

    /// <summary>The members, in the order they are written.</summary>
    public IReadOnlyDictionary<string, CanonicalValue> Members => _members;

    /// <summary>Builds an object from its members, refusing a duplicate key.</summary>
    public static CanonicalObject Create(IEnumerable<KeyValuePair<string, CanonicalValue>> members)
    {
        ArgumentNullException.ThrowIfNull(members);
        var sorted = new SortedDictionary<string, CanonicalValue>(StringComparer.Ordinal);
        foreach (var member in members)
        {
            if (string.IsNullOrEmpty(member.Key))
            {
                throw new ArgumentException("A canonical JSON key is never empty.", nameof(members));
            }

            if (!sorted.TryAdd(member.Key, member.Value ?? throw new ArgumentException(
                    "A canonical JSON member has a value.",
                    nameof(members))))
            {
                throw new ArgumentException("The key '" + member.Key + "' appears twice.", nameof(members));
            }
        }

        return new CanonicalObject(sorted);
    }

    /// <summary>Builds an object from tuples, in the order given (the order is not signed).</summary>
    public static CanonicalObject Of(params (string Key, CanonicalValue Value)[] members)
    {
        ArgumentNullException.ThrowIfNull(members);
        return Create(members.Select(static m => new KeyValuePair<string, CanonicalValue>(m.Key, m.Value)));
    }
}

/// <summary>A canonical JSON array, written in the order given: callers sort file lists by path.</summary>
public sealed class CanonicalArray : CanonicalValue
{
    private CanonicalArray(IReadOnlyList<CanonicalValue> items)
    {
        Items = items;
    }

    /// <summary>The items.</summary>
    public IReadOnlyList<CanonicalValue> Items { get; }

    /// <summary>An array of the given items.</summary>
    public static CanonicalArray Of(IEnumerable<CanonicalValue> items)
    {
        ArgumentNullException.ThrowIfNull(items);
        return new CanonicalArray(items.ToArray());
    }
}

/// <summary>A canonical JSON string.</summary>
public sealed class CanonicalString : CanonicalValue
{
    /// <summary>Wraps a string.</summary>
    public CanonicalString(string value)
    {
        Value = value ?? throw new ArgumentNullException(nameof(value));
    }

    /// <summary>The text.</summary>
    public string Value { get; }
}

/// <summary>A canonical JSON integer (there is no float form on purpose).</summary>
public sealed class CanonicalInteger : CanonicalValue
{
    /// <summary>Wraps an integer.</summary>
    public CanonicalInteger(long value)
    {
        Value = value;
    }

    /// <summary>The number.</summary>
    public long Value { get; }
}

/// <summary>A canonical JSON boolean.</summary>
public sealed class CanonicalBoolean : CanonicalValue
{
    /// <summary>Wraps a boolean.</summary>
    public CanonicalBoolean(bool value)
    {
        Value = value;
    }

    /// <summary>The value.</summary>
    public bool Value { get; }
}

/// <summary>
/// Writes and encodes canonical JSON, and holds the domain prefixes of the signing scheme: the
/// signed bytes are always <c>domain || canonical JSON</c>, never the raw file on disk.
/// </summary>
public static class CanonicalJson
{
    /// <summary>The domain prefix of a component package manifest's signed bytes.</summary>
    public const string ComponentManifestDomain = "SPOT-COMPONENT-MANIFEST-V1\n";

    /// <summary>The domain prefix of a publisher certificate's signed bytes (the Rust scheme).</summary>
    public const string CertificateDomain = "SPOT-PUBLISHER-CERT-V1\n";

    /// <summary>The domain prefix of a revocation list's signed bytes (the Rust scheme).</summary>
    public const string RevocationDomain = "SPOT-REVOCATION-LIST-V1\n";

    /// <summary>Shorthand for a string value.</summary>
    public static CanonicalValue Text(string value) => new CanonicalString(value);

    /// <summary>Shorthand for an integer value.</summary>
    public static CanonicalValue Number(long value) => new CanonicalInteger(value);

    /// <summary>Shorthand for a boolean value.</summary>
    public static CanonicalValue Flag(bool value) => new CanonicalBoolean(value);

    /// <summary>Writes <paramref name="value"/> as canonical JSON text.</summary>
    public static string Write(CanonicalValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        var builder = new StringBuilder(256);
        WriteValue(builder, value);
        return builder.ToString();
    }

    /// <summary>The signed bytes for <paramref name="domain"/> over <paramref name="value"/>.</summary>
    public static byte[] Encode(string domain, CanonicalValue value)
    {
        ArgumentException.ThrowIfNullOrEmpty(domain);
        var json = Encoding.UTF8.GetBytes(Write(value));
        var domainBytes = Encoding.UTF8.GetBytes(domain);
        var signed = new byte[domainBytes.Length + json.Length];
        domainBytes.CopyTo(signed, 0);
        json.CopyTo(signed, domainBytes.Length);
        return signed;
    }

    /// <summary>Writes a string the way serde_json writes one.</summary>
    public static void WriteString(StringBuilder builder, string value)
    {
        ArgumentNullException.ThrowIfNull(builder);
        ArgumentNullException.ThrowIfNull(value);
        builder.Append('"');
        foreach (var rune in value.EnumerateRunes())
        {
            switch (rune.Value)
            {
                case '"':
                    builder.Append("\\\"");
                    break;
                case '\\':
                    builder.Append("\\\\");
                    break;
                case '\b':
                    builder.Append("\\b");
                    break;
                case '\f':
                    builder.Append("\\f");
                    break;
                case '\n':
                    builder.Append("\\n");
                    break;
                case '\r':
                    builder.Append("\\r");
                    break;
                case '\t':
                    builder.Append("\\t");
                    break;
                default:
                    if (rune.Value < 0x20)
                    {
                        builder.Append("\\u").Append(rune.Value.ToString("x4", CultureInfo.InvariantCulture));
                    }
                    else
                    {
                        builder.Append(rune.ToString());
                    }

                    break;
            }
        }

        builder.Append('"');
    }

    private static void WriteValue(StringBuilder builder, CanonicalValue value)
    {
        switch (value)
        {
            case CanonicalObject obj:
                builder.Append('{');
                var first = true;
                foreach (var member in obj.Members)
                {
                    if (!first)
                    {
                        builder.Append(',');
                    }

                    first = false;
                    WriteString(builder, member.Key);
                    builder.Append(':');
                    WriteValue(builder, member.Value);
                }

                builder.Append('}');
                break;
            case CanonicalArray array:
                builder.Append('[');
                for (var i = 0; i < array.Items.Count; i++)
                {
                    if (i > 0)
                    {
                        builder.Append(',');
                    }

                    WriteValue(builder, array.Items[i]);
                }

                builder.Append(']');
                break;
            case CanonicalString text:
                WriteString(builder, text.Value);
                break;
            case CanonicalInteger number:
                builder.Append(number.Value.ToString(CultureInfo.InvariantCulture));
                break;
            case CanonicalBoolean flag:
                builder.Append(flag.Value ? "true" : "false");
                break;
            default:
                throw new ArgumentOutOfRangeException(nameof(value), value, "Unknown canonical JSON value.");
        }
    }
}
