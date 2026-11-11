using System.Globalization;

namespace Spotledger.Component.Host;

/// <summary>
/// The host's trust material, supplied by the store registry: the store roots, the publisher
/// certificates those roots issued, and the current signed revocation list. It holds public keys and
/// signatures only — never a secret — and it reads no clock: every check takes <c>now</c>.
/// <para>
/// Rotation is a second certificate for the same publisher with a new key and its own window: a
/// package signed before the roll still verifies while the old certificate's window covers
/// <c>signed_at</c>, and a package signed by the new key verifies as soon as the store has shipped
/// the new certificate. Revocation is the bundle-signing mechanism unchanged: the list is signed by
/// a store root, its generation strictly increases, and a list older than the one this store already
/// holds is refused.
/// </para>
/// </summary>
public sealed class ComponentTrustStore
{
    private readonly Dictionary<string, TrustRoot> _roots = new(StringComparer.Ordinal);
    private readonly Dictionary<string, SignedPublisherCertificate> _byKeyId = new(StringComparer.Ordinal);
    private readonly List<SignedPublisherCertificate> _certificates = new();

    private bool _hasList;

    /// <summary>
    /// Raised after a revocation list was accepted and is in force. The installation service listens
    /// and re-checks everything installed, so a revoked component cannot stay installed until a restart.
    /// </summary>
    public event EventHandler? RevocationAccepted;

    /// <summary>The highest revocation generation this store has accepted.</summary>
    public ulong HeldRevocationGeneration { get; private set; }

    /// <summary>The revocation list in force; empty when the store has shipped none.</summary>
    public RevocationBody? Revocations { get; private set; }

    /// <summary>The roots this store trusts.</summary>
    public IReadOnlyCollection<TrustRoot> Roots => _roots.Values;

    /// <summary>The publisher certificates this store has accepted.</summary>
    public IReadOnlyList<SignedPublisherCertificate> Certificates => _certificates;

    /// <summary>Adds a store root.</summary>
    /// <param name="root">The root.</param>
    public void AddRoot(TrustRoot root)
    {
        ArgumentNullException.ThrowIfNull(root);
        if (root.PublicKey.Length != Ed25519.PublicKeyBytes)
        {
            throw new ArgumentException("A store root's public key is 32 bytes.", nameof(root));
        }

        if (!string.Equals(Ed25519.KeyIdOf(root.PublicKey), root.KeyId, StringComparison.Ordinal))
        {
            throw new ArgumentException(
                "The root's key id " + root.KeyId + " is not its key's id " + Ed25519.KeyIdOf(root.PublicKey) + ".",
                nameof(root));
        }

        _roots[root.KeyId] = root;
    }

    /// <summary>Adds a store root from its raw public key, deriving the key id.</summary>
    /// <param name="publicKey">The 32-byte Ed25519 public key.</param>
    /// <param name="validFrom">First instant (UTC seconds) the root is trusted.</param>
    /// <param name="validTo">First instant the root is no longer trusted.</param>
    public TrustRoot AddRoot(byte[] publicKey, long validFrom, long validTo)
    {
        ArgumentNullException.ThrowIfNull(publicKey);
        var root = new TrustRoot(Ed25519.KeyIdOf(publicKey), publicKey, validFrom, validTo);
        AddRoot(root);
        return root;
    }

    /// <summary>
    /// Accepts a publisher certificate after checking that a trusted root issued it: the root named
    /// by <c>issued_by</c> must be known and valid at <paramref name="now"/>, and the root's signature
    /// over the body must verify.
    /// </summary>
    /// <param name="certificate">The certificate.</param>
    /// <param name="now">The instant to check the root's window at.</param>
    public ComponentRefusal? Trust(SignedPublisherCertificate certificate, long now)
    {
        ArgumentNullException.ThrowIfNull(certificate);
        if (certificate.Body.PublicKey.Length != Ed25519.PublicKeyBytes
            || certificate.Signature.Length != Ed25519.SignatureBytes)
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.CertificateSignatureInvalid,
                "certificate for '" + certificate.Body.PublisherId + "' is malformed (a 32-byte key and a 64-byte signature).");
        }

        if (!_roots.TryGetValue(certificate.Body.IssuedBy, out var root))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.UnknownRoot,
                "certificate for '" + certificate.Body.PublisherId + "' was issued by " + certificate.Body.IssuedBy
                + ", which is not a trusted store root.");
        }

        if (!root.ValidAt(now))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.UnknownRoot,
                "the store root " + root.KeyId + " is not trusted at " + now.ToString(CultureInfo.InvariantCulture)
                + " (its window is " + root.ValidFrom.ToString(CultureInfo.InvariantCulture) + ".."
                + root.ValidTo.ToString(CultureInfo.InvariantCulture) + ").");
        }

        if (!Ed25519.Verify(root.PublicKey, certificate.Body.SignedBytes(), certificate.Signature))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.CertificateSignatureInvalid,
                "the signature over the certificate for '" + certificate.Body.PublisherId
                + "' does not verify under the store root " + root.KeyId + ".");
        }

        _certificates.Add(certificate);
        _byKeyId[certificate.Body.KeyId] = certificate;
        return null;
    }

    /// <summary>Finds the certificate that names <paramref name="keyId"/>.</summary>
    /// <param name="keyId">The signing key id.</param>
    public bool TryFindCertificate(string keyId, out SignedPublisherCertificate? certificate)
    {
        certificate = null;
        return keyId is not null && _byKeyId.TryGetValue(keyId, out certificate);
    }

    /// <summary>Every certificate of one publisher, in the order the store shipped them (rotation).</summary>
    /// <param name="publisherId">The publisher.</param>
    public IReadOnlyList<SignedPublisherCertificate> CertificatesFor(string publisherId) =>
        _certificates.Where(c => string.Equals(c.Body.PublisherId, publisherId, StringComparison.Ordinal)).ToArray();

    /// <summary>
    /// Accepts a revocation list after checking its root's signature, and refuses one whose generation
    /// is not strictly above the one this store already holds (lower: stale; equal: not newer).
    /// </summary>
    /// <param name="list">The signed list.</param>
    /// <param name="now">The instant to check the signing root's window at.</param>
    public ComponentRefusal? AcceptRevocationList(RevocationList list, long now)
    {
        ArgumentNullException.ThrowIfNull(list);
        if (list.Signature.Length != Ed25519.SignatureBytes)
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.RevocationSignatureInvalid,
                "the revocation list's signature is not 64 bytes.");
        }

        if (!_roots.TryGetValue(list.Body.IssuedBy, out var root))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.UnknownRoot,
                "the revocation list was issued by " + list.Body.IssuedBy + ", which is not a trusted store root.");
        }

        if (!root.ValidAt(now))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.UnknownRoot,
                "the store root " + root.KeyId + " is not trusted at " + now.ToString(CultureInfo.InvariantCulture) + ".");
        }

        if (!Ed25519.Verify(root.PublicKey, list.Body.SignedBytes(), list.Signature))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.RevocationSignatureInvalid,
                "the revocation list's signature does not verify under the store root " + root.KeyId + ".");
        }

        if (_hasList && list.Body.Generation <= HeldRevocationGeneration)
        {
            var equal = list.Body.Generation == HeldRevocationGeneration;
            return new ComponentRefusal(
                equal ? ComponentRefusalCodes.GenerationNotNewer : ComponentRefusalCodes.StaleRevocationList,
                "the revocation list is generation " + list.Body.Generation.ToString(CultureInfo.InvariantCulture)
                + (equal ? ", the same as" : ", older than")
                + " the generation " + HeldRevocationGeneration.ToString(CultureInfo.InvariantCulture)
                + " this store already holds: only a strictly higher generation replaces it, "
                + "so a list with other contents at an equal generation is refused too.");
        }

        Revocations = list.Body;
        HeldRevocationGeneration = list.Body.Generation;
        _hasList = true;
        RevocationAccepted?.Invoke(this, EventArgs.Empty);
        return null;
    }
}
