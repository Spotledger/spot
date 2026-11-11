using System.Globalization;

namespace Spotledger.Component.Host;

/// <summary>
/// A publisher certificate body, exactly as <c>crates/framework/spot-bundle-signing/src/certificate.rs</c>
/// defines it: the publisher's Ed25519 key, named and dated, issued by a store root. Its signed bytes
/// are <see cref="CanonicalJson.CertificateDomain"/> followed by the canonical JSON of
/// <see cref="ToCanonical"/>, so a certificate the Rust store issued verifies here unchanged.
/// </summary>
/// <param name="PublisherId">The publisher's stable id.</param>
/// <param name="DisplayName">The name shown to an admin.</param>
/// <param name="PublicKey">The publisher's 32-byte Ed25519 public key.</param>
/// <param name="ValidFrom">First instant (UTC seconds) the certificate is valid.</param>
/// <param name="ValidTo">First instant the certificate is no longer valid.</param>
/// <param name="IssuedBy">The key id of the store root that issued it.</param>
/// <param name="Serial">The issuer's serial number for this certificate.</param>
public sealed record PublisherCertificateBody(
    string PublisherId,
    string DisplayName,
    byte[] PublicKey,
    long ValidFrom,
    long ValidTo,
    string IssuedBy,
    long Serial)
{
    /// <summary>The key id of the publisher's key.</summary>
    public string KeyId => Ed25519.KeyIdOf(PublicKey);

    /// <summary>True when the certificate's window covers <paramref name="now"/>.</summary>
    /// <param name="now">The instant to check, supplied by the caller (the host never reads a clock).</param>
    public bool ValidAt(long now) => now >= ValidFrom && now < ValidTo;

    /// <summary>The canonical JSON the issuing root signs, without the domain prefix.</summary>
    public CanonicalValue ToCanonical() =>
        CanonicalObject.Of(
            ("display_name", CanonicalJson.Text(DisplayName)),
            ("issued_by", CanonicalJson.Text(IssuedBy)),
            ("publisher_id", CanonicalJson.Text(PublisherId)),
            ("public_key", CanonicalJson.Text(Ed25519.ToBase64(PublicKey))),
            ("serial", CanonicalJson.Number(Serial)),
            ("valid_from", CanonicalJson.Number(ValidFrom)),
            ("valid_to", CanonicalJson.Number(ValidTo)));

    /// <summary>The exact bytes the store root's Ed25519 signature covers.</summary>
    public byte[] SignedBytes() => CanonicalJson.Encode(CanonicalJson.CertificateDomain, ToCanonical());

    /// <inheritdoc />
    public override string ToString() => PublisherId + " (" + KeyId + ") " + ValidFrom.ToString(CultureInfo.InvariantCulture)
        + ".." + ValidTo.ToString(CultureInfo.InvariantCulture);
}

/// <summary>A publisher certificate with the issuing root's signature over its body.</summary>
/// <param name="Body">The body.</param>
/// <param name="Signature">The root's 64-byte Ed25519 signature.</param>
public sealed record SignedPublisherCertificate(PublisherCertificateBody Body, byte[] Signature);

/// <summary>
/// A store root: the public key and the window it is trusted in. The store registry supplies these;
/// the host reads no key material of its own and holds no secret.
/// </summary>
/// <param name="KeyId">The root's key id.</param>
/// <param name="PublicKey">The root's 32-byte Ed25519 public key.</param>
/// <param name="ValidFrom">First instant (UTC seconds) the root is trusted.</param>
/// <param name="ValidTo">First instant the root is no longer trusted.</param>
public sealed record TrustRoot(string KeyId, byte[] PublicKey, long ValidFrom, long ValidTo)
{
    /// <summary>True when the root is trusted at <paramref name="now"/>, and its id matches its key.</summary>
    /// <param name="now">The instant to check.</param>
    public bool ValidAt(long now) =>
        now >= ValidFrom && now < ValidTo && string.Equals(Ed25519.KeyIdOf(PublicKey), KeyId, StringComparison.Ordinal);

    /// <inheritdoc />
    public override string ToString() => KeyId + " " + ValidFrom.ToString(CultureInfo.InvariantCulture)
        + ".." + ValidTo.ToString(CultureInfo.InvariantCulture);
}

/// <summary>A revoked signing key: every signature it made is refused (see <see cref="RevocationBody.CheckKey"/>).</summary>
/// <param name="KeyId">The key.</param>
/// <param name="EffectiveFrom">Shown to an admin and covered by the list signature; no decision rests on it.</param>
/// <param name="Compromised">True when every signature the key ever made is refused.</param>
/// <param name="Reason">Why, for the admin.</param>
public sealed record RevokedKeyEntry(string KeyId, long EffectiveFrom, bool Compromised, string Reason);

/// <summary>A revoked publisher: everything it published is refused.</summary>
/// <param name="PublisherId">The publisher.</param>
/// <param name="EffectiveFrom">Shown to an admin and covered by the list signature; no decision rests on it.</param>
/// <param name="Reason">Why, for the admin.</param>
public sealed record RevokedPublisherEntry(string PublisherId, long EffectiveFrom, string Reason);

/// <summary>A revoked package, by the digest of its signed manifest bytes.</summary>
/// <param name="Digest">The digest in lower-case hex.</param>
/// <param name="Reason">Why, for the admin.</param>
public sealed record RevokedPackageEntry(string Digest, string Reason);

/// <summary>
/// The body of a signed revocation list, exactly as <c>revocation.rs</c> defines it. The generation
/// strictly increases; a verifier refuses a list older than the one it already holds.
/// </summary>
/// <param name="Generation">Strictly increasing per issued list.</param>
/// <param name="IssuedAt">When the list was issued (UTC seconds).</param>
/// <param name="IssuedBy">The key id of the root that signs it.</param>
/// <param name="Keys">The revoked keys, sorted by key id before signing.</param>
/// <param name="Publishers">The revoked publishers, sorted by publisher id before signing.</param>
/// <param name="Packages">The revoked package digests, sorted before signing.</param>
public sealed class RevocationBody
{
    /// <summary>Creates a body; entries are sorted here, so equal content signs the same bytes.</summary>
    public RevocationBody(
        ulong generation,
        long issuedAt,
        string issuedBy,
        IEnumerable<RevokedKeyEntry> keys,
        IEnumerable<RevokedPublisherEntry> publishers,
        IEnumerable<RevokedPackageEntry> packages)
    {
        Generation = generation;
        IssuedAt = issuedAt;
        IssuedBy = issuedBy ?? throw new ArgumentNullException(nameof(issuedBy));
        Keys = (keys ?? throw new ArgumentNullException(nameof(keys)))
            .OrderBy(static k => k.KeyId, StringComparer.Ordinal).ToArray();
        Publishers = (publishers ?? throw new ArgumentNullException(nameof(publishers)))
            .OrderBy(static p => p.PublisherId, StringComparer.Ordinal).ToArray();
        Packages = (packages ?? throw new ArgumentNullException(nameof(packages)))
            .OrderBy(static p => p.Digest, StringComparer.Ordinal).ToArray();
    }

    /// <summary>Strictly increasing per issued list.</summary>
    public ulong Generation { get; }

    /// <summary>When the list was issued (UTC seconds).</summary>
    public long IssuedAt { get; }

    /// <summary>The key id of the root that signs it.</summary>
    public string IssuedBy { get; }

    /// <summary>The revoked keys, sorted by key id.</summary>
    public IReadOnlyList<RevokedKeyEntry> Keys { get; }

    /// <summary>The revoked publishers, sorted by publisher id.</summary>
    public IReadOnlyList<RevokedPublisherEntry> Publishers { get; }

    /// <summary>The revoked package digests, sorted.</summary>
    public IReadOnlyList<RevokedPackageEntry> Packages { get; }

    /// <summary>The canonical JSON the root signs, without the domain prefix.</summary>
    public CanonicalValue ToCanonical() =>
        CanonicalObject.Of(
            ("bundles", CanonicalArray.Of(Packages.Select(static p => CanonicalObject.Of(
                ("digest", CanonicalJson.Text(p.Digest)),
                ("reason", CanonicalJson.Text(p.Reason)))))),
            ("generation", CanonicalJson.Number(checked((long)Generation))),
            ("issued_at", CanonicalJson.Number(IssuedAt)),
            ("issued_by", CanonicalJson.Text(IssuedBy)),
            ("keys", CanonicalArray.Of(Keys.Select(static k => CanonicalObject.Of(
                ("compromised", CanonicalJson.Flag(k.Compromised)),
                ("effective_from", CanonicalJson.Number(k.EffectiveFrom)),
                ("key_id", CanonicalJson.Text(k.KeyId)),
                ("reason", CanonicalJson.Text(k.Reason)))))),
            ("publishers", CanonicalArray.Of(Publishers.Select(static p => CanonicalObject.Of(
                ("effective_from", CanonicalJson.Number(p.EffectiveFrom)),
                ("publisher_id", CanonicalJson.Text(p.PublisherId)),
                ("reason", CanonicalJson.Text(p.Reason)))))));

    /// <summary>The exact bytes the issuing root's Ed25519 signature covers.</summary>
    public byte[] SignedBytes() => CanonicalJson.Encode(CanonicalJson.RevocationDomain, ToCanonical());

    /// <summary>
    /// The refusal for a signing key the list names, or null. A listed key refuses every signature it
    /// made, whatever the signature claims about its own date: <c>signed_at</c> is written by the
    /// publisher, so a compromised publisher key could backdate and slip under an "effective from"
    /// rule. The <c>compromised</c> flag only picks the code. <c>effective_from</c> stays in the wire
    /// format (the signature covers it) and is shown to an admin; it decides nothing.
    /// </summary>
    /// <param name="keyId">The signing key.</param>
    /// <param name="compromised">True when the list flags the key as compromised.</param>
    public ComponentRefusal? CheckKey(string keyId, out bool compromised)
    {
        compromised = false;
        foreach (var entry in Keys.Where(k => string.Equals(k.KeyId, keyId, StringComparison.Ordinal)))
        {
            if (entry.Compromised)
            {
                compromised = true;
                return new ComponentRefusal(
                    ComponentRefusalCodes.KeyCompromised,
                    "the signing key " + keyId + " was reported compromised (" + entry.Reason
                    + "); every signature it made is refused.");
            }

            return new ComponentRefusal(
                ComponentRefusalCodes.KeyRevoked,
                "the signing key " + keyId + " was revoked (" + entry.Reason
                + "); every signature it made is refused, whatever date the package claims.");
        }

        return null;
    }

    /// <summary>The refusal for a publisher the list names, or null: everything it published is refused.</summary>
    /// <param name="publisherId">The publisher.</param>
    public ComponentRefusal? CheckPublisher(string publisherId)
    {
        foreach (var entry in Publishers.Where(p => string.Equals(p.PublisherId, publisherId, StringComparison.Ordinal)))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.PublisherRevoked,
                "the publisher '" + publisherId + "' was revoked (" + entry.Reason
                + "); everything it published is refused.");
        }

        return null;
    }

    /// <summary>
    /// The first revocation that applies to a package: its publisher, its signing key, then its
    /// digest. Verification and the re-check of installed components both call this, so they cannot
    /// disagree.
    /// </summary>
    /// <param name="publisherId">The publisher.</param>
    /// <param name="keyId">The signing key.</param>
    /// <param name="digest">The digest of the signed manifest bytes, lower-case hex.</param>
    public ComponentRefusal? CheckPackageFacts(string publisherId, string keyId, string digest) =>
        CheckPublisher(publisherId) ?? CheckKey(keyId, out _) ?? CheckPackage(digest);

    /// <summary>True when the package digest is revoked.</summary>
    /// <param name="digest">The digest of the signed manifest bytes, lower-case hex.</param>
    public ComponentRefusal? CheckPackage(string digest)
    {
        foreach (var entry in Packages.Where(p => string.Equals(p.Digest, digest, StringComparison.Ordinal)))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.PackageRevoked,
                "the package " + digest + " was withdrawn (" + entry.Reason + ").");
        }

        return null;
    }
}

/// <summary>A revocation list with the store root's signature.</summary>
/// <param name="Body">The body.</param>
/// <param name="Signature">The root's 64-byte Ed25519 signature.</param>
public sealed record RevocationList(RevocationBody Body, byte[] Signature);
