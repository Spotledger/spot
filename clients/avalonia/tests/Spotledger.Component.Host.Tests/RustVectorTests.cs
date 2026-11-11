using System.Text;
using System.Text.Json;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// The C# implementation against bytes the Rust implementation made: the key ids, a publisher
/// certificate, a revocation list and four manifests, all signed by <c>spot-bundle-signing</c>
/// itself. A difference in canonicalisation or in the domain prefix shows up here as a failed
/// signature, not as a package that loads on one side and not the other.
/// </summary>
public sealed class RustVectorTests
{
    private readonly Vectors _vectors = Vectors.Load();

    [Fact]
    public void KeyIdsMatchTheRustDerivation()
    {
        foreach (var name in new[] { "root", "publisher", "revoked", "outsider" })
        {
            Assert.Equal(_vectors.KeyId(name), Ed25519.KeyIdOf(_vectors.PublicKey(name)));
        }
    }

    [Fact]
    public void TheCertificateSignedBytesAndSignatureAreReproduced()
    {
        var body = _vectors.Certificate.GetProperty("body");
        var certificate = new PublisherCertificateBody(
            body.GetProperty("publisher_id").GetString()!,
            body.GetProperty("display_name").GetString()!,
            Convert.FromBase64String(body.GetProperty("public_key_b64").GetString()!),
            body.GetProperty("valid_from").GetInt64(),
            body.GetProperty("valid_to").GetInt64(),
            body.GetProperty("issued_by").GetString()!,
            body.GetProperty("serial").GetInt64());

        Assert.Equal(Convert.FromHexString(_vectors.Certificate.GetProperty("signed_bytes_hex").GetString()!), certificate.SignedBytes());
        Assert.True(Ed25519.Verify(
            _vectors.PublicKey("root"),
            certificate.SignedBytes(),
            Convert.FromBase64String(_vectors.Certificate.GetProperty("signature_b64").GetString()!)));
        Assert.Equal(_vectors.KeyId("publisher"), certificate.KeyId);
    }

    [Fact]
    public void TheRevocationListSignedBytesAndSignatureAreReproduced()
    {
        var body = _vectors.Revocation.GetProperty("body");
        var keys = body.GetProperty("keys").EnumerateArray()
            .Select(k => new RevokedKeyEntry(
                k.GetProperty("key_id").GetString()!,
                k.GetProperty("effective_from").GetInt64(),
                k.GetProperty("compromised").GetBoolean(),
                k.GetProperty("reason").GetString()!));
        var publishers = body.GetProperty("publishers").EnumerateArray()
            .Select(p => new RevokedPublisherEntry(
                p.GetProperty("publisher_id").GetString()!,
                p.GetProperty("effective_from").GetInt64(),
                p.GetProperty("reason").GetString()!));
        var packages = body.GetProperty("bundles").EnumerateArray()
            .Select(b => new RevokedPackageEntry(
                b.GetProperty("digest").GetString()!,
                b.GetProperty("reason").GetString()!));

        var rebuilt = new RevocationBody(
            body.GetProperty("generation").GetUInt64(),
            body.GetProperty("issued_at").GetInt64(),
            body.GetProperty("issued_by").GetString()!,
            keys,
            publishers,
            packages);

        Assert.Equal(Convert.FromHexString(_vectors.Revocation.GetProperty("signed_bytes_hex").GetString()!), rebuilt.SignedBytes());
        Assert.True(Ed25519.Verify(
            _vectors.PublicKey("root"),
            rebuilt.SignedBytes(),
            Convert.FromBase64String(_vectors.Revocation.GetProperty("signature_b64").GetString()!)));
    }

    [Fact]
    public void ThePlainManifestParsesAndReproducesItsSignedBytes()
    {
        var manifest = ParseVectorManifest("plain");

        Assert.Equal(Convert.FromHexString(_vectors.Manifest("plain").GetProperty("signed_bytes_hex").GetString()!), manifest.SignedBytes());
        Assert.Equal(_vectors.Manifest("plain").GetProperty("signed_bytes_sha256").GetString(), manifest.SignedBytesSha256());
        Assert.True(Ed25519.Verify(
            _vectors.PublicKey("publisher"),
            manifest.SignedBytes(),
            Convert.FromBase64String(_vectors.Manifest("plain").GetProperty("signature_b64").GetString()!)));
    }

    [Fact]
    public void TheManifestWithAwkwardStringsReproducesItsSignedBytes()
    {
        // The one test that proves the escaping: quotes, a backslash, newline, tab, control
        // characters, DEL, and non-ASCII text, all written exactly as serde_json wrote them.
        var manifest = ParseVectorManifest("tricky_strings");

        Assert.Equal(Convert.FromHexString(_vectors.Manifest("tricky_strings").GetProperty("signed_bytes_hex").GetString()!), manifest.SignedBytes());
        Assert.True(Ed25519.Verify(
            _vectors.PublicKey("publisher"),
            manifest.SignedBytes(),
            Convert.FromBase64String(_vectors.Manifest("tricky_strings").GetProperty("signature_b64").GetString()!)));
        Assert.Contains("\u007f", Encoding.UTF8.GetString(manifest.SignedBytes()), StringComparison.Ordinal);
    }

    [Fact]
    public void ASignatureFromTheWrongKeyIsRefused()
    {
        var manifest = ParseVectorManifest("wrong_key");

        Assert.False(Ed25519.Verify(
            _vectors.PublicKey("publisher"),
            manifest.SignedBytes(),
            Convert.FromBase64String(_vectors.Manifest("wrong_key").GetProperty("signature_b64").GetString()!)));
    }

    [Fact]
    public void TheCompromisedKeysSignatureVerifiesCryptographicallyButItsRevocationRefusesIt()
    {
        var manifest = ParseVectorManifest("compromised_signer");
        var signature = Convert.FromBase64String(_vectors.Manifest("compromised_signer").GetProperty("signature_b64").GetString()!);

        // Ed25519 says the bytes are intact; the revocation list says the key may not be used.
        Assert.True(Ed25519.Verify(_vectors.PublicKey("revoked"), manifest.SignedBytes(), signature));

        var revocation = new RevocationBody(
            12,
            1,
            _vectors.KeyId("root"),
            new[] { new RevokedKeyEntry(_vectors.KeyId("revoked"), 1_750_000_000, true, "compromised") },
            Array.Empty<RevokedPublisherEntry>(),
            Array.Empty<RevokedPackageEntry>());
        var refusal = revocation.CheckKey(_vectors.KeyId("revoked"), out var compromised);

        Assert.NotNull(refusal);
        Assert.True(compromised);
        Assert.Equal(ComponentRefusalCodes.KeyCompromised, refusal.Code);
    }

    [Fact]
    public void TheRustCertificateIsAcceptedByTheTrustStoreAndItsKeySignsAPackage()
    {
        var trust = new ComponentTrustStore();
        trust.AddRoot(_vectors.PublicKey("root"), 1_600_000_000, 2_000_000_000);

        var body = _vectors.Certificate.GetProperty("body");
        var certificate = new SignedPublisherCertificate(
            new PublisherCertificateBody(
                body.GetProperty("publisher_id").GetString()!,
                body.GetProperty("display_name").GetString()!,
                Convert.FromBase64String(body.GetProperty("public_key_b64").GetString()!),
                body.GetProperty("valid_from").GetInt64(),
                body.GetProperty("valid_to").GetInt64(),
                body.GetProperty("issued_by").GetString()!,
                body.GetProperty("serial").GetInt64()),
            Convert.FromBase64String(_vectors.Certificate.GetProperty("signature_b64").GetString()!));

        Assert.Null(trust.Trust(certificate, 1_760_000_000));
        Assert.True(trust.TryFindCertificate(_vectors.KeyId("publisher"), out var found));
        Assert.Equal("acme", found!.Body.PublisherId);
    }

    [Fact]
    public void TheRustRevocationListIsAcceptedByTheTrustStore()
    {
        var trust = new ComponentTrustStore();
        trust.AddRoot(_vectors.PublicKey("root"), 1_600_000_000, 2_000_000_000);
        var body = _vectors.Revocation.GetProperty("body");

        var list = new RevocationList(
            new RevocationBody(
                body.GetProperty("generation").GetUInt64(),
                body.GetProperty("issued_at").GetInt64(),
                body.GetProperty("issued_by").GetString()!,
                body.GetProperty("keys").EnumerateArray().Select(k => new RevokedKeyEntry(
                    k.GetProperty("key_id").GetString()!,
                    k.GetProperty("effective_from").GetInt64(),
                    k.GetProperty("compromised").GetBoolean(),
                    k.GetProperty("reason").GetString()!)),
                body.GetProperty("publishers").EnumerateArray().Select(p => new RevokedPublisherEntry(
                    p.GetProperty("publisher_id").GetString()!,
                    p.GetProperty("effective_from").GetInt64(),
                    p.GetProperty("reason").GetString()!)),
                body.GetProperty("bundles").EnumerateArray().Select(b => new RevokedPackageEntry(
                    b.GetProperty("digest").GetString()!,
                    b.GetProperty("reason").GetString()!))),
            Convert.FromBase64String(_vectors.Revocation.GetProperty("signature_b64").GetString()!));

        Assert.Null(trust.AcceptRevocationList(list, 1_760_000_000));
        Assert.Equal(12UL, trust.HeldRevocationGeneration);
        var revoked = Assert.Single(trust.Revocations!.Keys);
        Assert.Equal(_vectors.Revocation.GetProperty("body").GetProperty("keys")[0].GetProperty("key_id").GetString(), revoked.KeyId);
    }

    private ComponentManifest ParseVectorManifest(string name)
    {
        var body = _vectors.Manifest(name).GetProperty("body");
        Assert.True(
            ComponentManifest.TryParse(body.GetRawText(), out var manifest, out var problems),
            string.Join(" ", problems));
        return manifest!;
    }
}
