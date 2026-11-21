using System.Globalization;

namespace Spotledger.Client.Effects.Tests.Corpus;

/// <summary>
/// The deterministic ids the corpus's documents carry, mirroring
/// <c>crates/framework/spot-form-effects/src/runner/ids.rs</c>: FNV-1a over two lanes, shaped
/// as a UUIDv7. The Rust crate derives a document's id from its case id and each embedded
/// row's id from the document's, so the client can rebuild the same document from the same
/// corpus without a clock or a random source.
/// </summary>
internal static class StableIds
{
    private const ulong FnvOffset = 0xcbf2_9ce4_8422_2325;
    private const ulong FnvPrime = 0x0000_0100_0000_01b3;
    private const ulong SecondLane = 0x8422_2325_cbf2_9ce4;

    /// <summary>The id derived from <paramref name="seed"/>.</summary>
    public static string For(string seed)
    {
        ArgumentNullException.ThrowIfNull(seed);
        unchecked
        {
            ulong a = FnvOffset;
            ulong b = SecondLane;
            foreach (byte value in System.Text.Encoding.UTF8.GetBytes(seed))
            {
                a ^= value;
                a *= FnvPrime;
                b = ((b << 7) | (b >> 57)) ^ (value + a);
            }

            var bytes = new byte[16];
            WriteBigEndian(bytes, 0, a);
            WriteBigEndian(bytes, 8, b);
            bytes[6] = (byte)((bytes[6] & 0x0f) | 0x70);
            bytes[8] = (byte)((bytes[8] & 0x3f) | 0x80);
            return Format(bytes);
        }
    }

    /// <summary>The id of an embedded row: <c>{document}|row|{ordinal}</c>, from 0.</summary>
    public static string RowId(string documentId, int ordinal) =>
        For(string.Create(CultureInfo.InvariantCulture, $"{documentId}|row|{ordinal}"));

    private static void WriteBigEndian(byte[] target, int offset, ulong value)
    {
        for (int i = 0; i < 8; i++)
        {
            target[offset + i] = (byte)(value >> (56 - (8 * i)));
        }
    }

    private static string Format(byte[] bytes)
    {
        string hex = Convert.ToHexStringLower(bytes);
        return string.Create(
            CultureInfo.InvariantCulture,
            $"{hex[..8]}-{hex[8..12]}-{hex[12..16]}-{hex[16..20]}-{hex[20..]}");
    }
}
