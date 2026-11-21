using System.Globalization;
using System.Security.Cryptography;
using NSec.Cryptography;

namespace Spotledger.Component.Host;

/// <summary>
/// Ed25519 detached signatures and key ids over the 32-byte raw public key format
/// <c>spot-bundle-signing</c> uses. The BCL has no Ed25519, so this wraps NSec.Cryptography (the
/// same package the SPK-08 desktop probe ran on this platform); every other rule here — the key id
/// derivation, the 64-byte signature, the domain separation — is the Rust crate's.
/// </summary>
public static class Ed25519
{
    /// <summary>The bytes of an Ed25519 public key.</summary>
    public const int PublicKeyBytes = 32;

    /// <summary>The bytes of an Ed25519 signature.</summary>
    public const int SignatureBytes = 64;

    /// <summary>The characters of a key id: 32, the first 16 bytes of the key's SHA-256 in hex.</summary>
    public const int KeyIdCharacters = 32;

    /// <summary>
    /// The key id of a public key: the first 16 bytes of its SHA-256, lower-case hex. The same
    /// derivation <c>spot-bundle-signing</c> and <c>spot-signed-command</c> use, so one key id means
    /// one key everywhere.
    /// </summary>
    public static string KeyIdOf(ReadOnlySpan<byte> publicKey)
    {
        if (publicKey.Length != PublicKeyBytes)
        {
            throw new ArgumentException("An Ed25519 public key is 32 bytes.", nameof(publicKey));
        }

        var digest = SHA256.HashData(publicKey);
        return Convert.ToHexStringLower(digest.AsSpan(0, 16));
    }

    /// <summary>True when <paramref name="text"/> is a well-formed key id (32 lower-case hex characters).</summary>
    public static bool IsKeyId(string? text)
    {
        if (text is not { Length: KeyIdCharacters })
        {
            return false;
        }

        foreach (var character in text)
        {
            var hex = character is >= '0' and <= '9' or >= 'a' and <= 'f';
            if (!hex)
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>
    /// Verifies a detached signature. Returns false for a signature that does not verify and for a
    /// public key or signature that is not the right shape: a malformed input is not a special case
    /// a caller can act on, it is a refusal.
    /// </summary>
    public static bool Verify(ReadOnlySpan<byte> publicKey, ReadOnlySpan<byte> message, ReadOnlySpan<byte> signature)
    {
        if (publicKey.Length != PublicKeyBytes || signature.Length != SignatureBytes)
        {
            return false;
        }

        PublicKey imported;
        try
        {
            imported = PublicKey.Import(
                SignatureAlgorithm.Ed25519,
                publicKey.ToArray(),
                KeyBlobFormat.RawPublicKey);
        }
        catch (ArgumentException)
        {
            // Not a curve point: nothing can verify under it.
            return false;
        }
        catch (FormatException)
        {
            return false;
        }

        return SignatureAlgorithm.Ed25519.Verify(imported, message.ToArray(), signature.ToArray());
    }

    /// <summary>
    /// Signs with a 32-byte seed. This exists for tests and for the store's own package builder; a
    /// client host only ever verifies, so no secret enters the host's paths.
    /// </summary>
    /// <param name="seed">The 32-byte Ed25519 seed.</param>
    /// <param name="message">The bytes to sign.</param>
    /// <returns>The 64-byte signature.</returns>
    public static byte[] Sign(ReadOnlySpan<byte> seed, ReadOnlySpan<byte> message)
    {
        if (seed.Length != PublicKeyBytes)
        {
            throw new ArgumentException("An Ed25519 seed is 32 bytes.", nameof(seed));
        }

        var parameters = new KeyCreationParameters { ExportPolicy = KeyExportPolicies.AllowPlaintextExport };
        using var key = Key.Import(SignatureAlgorithm.Ed25519, seed.ToArray(), KeyBlobFormat.RawPrivateKey, parameters);
        return SignatureAlgorithm.Ed25519.Sign(key, message.ToArray());
    }

    /// <summary>The public key of a 32-byte seed, for a caller that holds only the seed.</summary>
    public static byte[] PublicKeyOf(ReadOnlySpan<byte> seed)
    {
        if (seed.Length != PublicKeyBytes)
        {
            throw new ArgumentException("An Ed25519 seed is 32 bytes.", nameof(seed));
        }

        var parameters = new KeyCreationParameters { ExportPolicy = KeyExportPolicies.AllowPlaintextExport };
        using var key = Key.Import(SignatureAlgorithm.Ed25519, seed.ToArray(), KeyBlobFormat.RawPrivateKey, parameters);
        return key.PublicKey.Export(KeyBlobFormat.RawPublicKey);
    }

    /// <summary>Lower-case hex of <paramref name="bytes"/>, the form every digest in the scheme takes.</summary>
    public static string ToHex(ReadOnlySpan<byte> bytes) => Convert.ToHexStringLower(bytes);

    /// <summary>Parses lower-case hex, returning false instead of throwing.</summary>
    public static bool TryFromHex(string? text, out byte[] bytes)
    {
        bytes = Array.Empty<byte>();
        if (string.IsNullOrEmpty(text) || text.Length % 2 != 0)
        {
            return false;
        }

        try
        {
            bytes = Convert.FromHexString(text);
            return true;
        }
        catch (FormatException)
        {
            return false;
        }
    }

    /// <summary>The SHA-256 of <paramref name="bytes"/> as lower-case hex.</summary>
    public static string Sha256Hex(ReadOnlySpan<byte> bytes) => Convert.ToHexStringLower(SHA256.HashData(bytes));

    /// <summary>Standard base64 of <paramref name="bytes"/>.</summary>
    public static string ToBase64(ReadOnlySpan<byte> bytes) => Convert.ToBase64String(bytes);

    /// <summary>Parses standard base64, returning false instead of throwing.</summary>
    public static bool TryFromBase64(string? text, out byte[] bytes)
    {
        bytes = Array.Empty<byte>();
        if (string.IsNullOrWhiteSpace(text))
        {
            return false;
        }

        var trimmed = text.Trim();
        var buffer = new byte[trimmed.Length];
        if (!Convert.TryFromBase64String(trimmed, buffer, out var written))
        {
            return false;
        }

        bytes = buffer.AsSpan(0, written).ToArray();
        return true;
    }

    /// <summary>An invariant rendering of <paramref name="value"/>, for messages.</summary>
    public static string Show(long value) => value.ToString(CultureInfo.InvariantCulture);
}
