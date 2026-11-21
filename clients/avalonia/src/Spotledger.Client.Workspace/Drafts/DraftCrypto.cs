using System.Security.Cryptography;
using System.Text;

namespace Spotledger.Client.Workspace;

/// <summary>
/// The draft file container: <c>"SPD1" | nonce(12) | tag(16) | ciphertext</c>, AES-256-GCM, with the
/// scope bound in as associated data so a file copied to another user's or another server's directory
/// does not open. The key is never in a file.
/// </summary>
internal static class DraftCrypto
{
    private static readonly byte[] Magic = "SPD1"u8.ToArray();
    internal const int KeyBytes = 32;
    private const int NonceBytes = 12;
    private const int TagBytes = 16;

    public static byte[] NewKey() => RandomNumberGenerator.GetBytes(KeyBytes);

    public static byte[] Seal(byte[] key, DraftScope scope, ReadOnlySpan<byte> plaintext)
    {
        var nonce = RandomNumberGenerator.GetBytes(NonceBytes);
        var output = new byte[Magic.Length + NonceBytes + TagBytes + plaintext.Length];
        Magic.CopyTo(output, 0);
        nonce.CopyTo(output, Magic.Length);
        var tag = output.AsSpan(Magic.Length + NonceBytes, TagBytes);
        var cipher = output.AsSpan(Magic.Length + NonceBytes + TagBytes);
        using var aes = new AesGcm(key, TagBytes);
        aes.Encrypt(nonce, plaintext, cipher, tag, Context(scope));
        return output;
    }

    /// <summary>Opens a file; throws <see cref="CryptographicException"/> for a wrong key, a wrong scope or a damaged file, <see cref="InvalidDataException"/> for something that is not a draft file.</summary>
    public static byte[] Open(byte[] key, DraftScope scope, ReadOnlySpan<byte> file)
    {
        if (file.Length < Magic.Length + NonceBytes + TagBytes || !file[..Magic.Length].SequenceEqual(Magic))
        {
            throw new InvalidDataException("Not a draft file.");
        }

        var nonce = file.Slice(Magic.Length, NonceBytes);
        var tag = file.Slice(Magic.Length + NonceBytes, TagBytes);
        var cipher = file[(Magic.Length + NonceBytes + TagBytes)..];
        var plaintext = new byte[cipher.Length];
        using var aes = new AesGcm(key, TagBytes);
        aes.Decrypt(nonce, cipher, tag, plaintext, Context(scope));
        return plaintext;
    }

    private static byte[] Context(DraftScope scope)
        => Encoding.UTF8.GetBytes("spot-draft-v1\u001f" + scope.ServerId.Trim().TrimEnd('/') + "\u001f" + scope.UserId.Trim());
}
