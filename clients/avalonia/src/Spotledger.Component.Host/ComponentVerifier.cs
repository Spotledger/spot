using System.Globalization;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>
/// The one verification a component package passes before any code is loaded. It is the same story
/// as <c>spot-bundle-signing::verify</c> for app bundles: a publisher certificate issued by a store
/// root, a detached Ed25519 signature over domain-separated canonical bytes, a signed revocation
/// list whose generation only moves forward, and no clock read anywhere — <c>now</c> is an argument.
/// <para>The checks run in this fixed order, and the first failing one decides the refusal:</para>
/// <list type="number">
/// <item>the signing key is one a trusted publisher certificate names;</item>
/// <item>that certificate names the manifest's publisher;</item>
/// <item>the certificate's window covers <c>now</c>;</item>
/// <item>the publisher, the signing key and the package digest are not revoked (a revoked or
/// compromised key is refused whatever date the manifest claims: <c>signed_at</c> is written by
/// the publisher, so no decision rests on it);</item>
/// <item>the Ed25519 signature verifies over the canonical manifest bytes;</item>
/// <item>the component's contract range contains this host's contract version.</item>
/// </list>
/// <para>
/// A revocation list older than the generation the trust store already holds never reaches this
/// function: <see cref="ComponentTrustStore.AcceptRevocationList"/> refuses the replay when the list
/// arrives, so the list in force only moves forward.
/// </para>
/// </summary>
public sealed class ComponentVerifier
{
    private readonly ComponentTrustStore _trust;
    private readonly ContractVersion _hostVersion;

    /// <summary>Creates a verifier over a trust store, for this host's contract version.</summary>
    /// <param name="trust">The store registry's trust material.</param>
    /// <param name="hostVersion">The contract version this host speaks; the contract's when null.</param>
    public ComponentVerifier(ComponentTrustStore trust, ContractVersion? hostVersion = null)
    {
        _trust = trust ?? throw new ArgumentNullException(nameof(trust));
        _hostVersion = hostVersion ?? ComponentContract.Version;
    }

    /// <summary>Verifies <paramref name="package"/> at <paramref name="now"/> (UTC seconds).</summary>
    /// <param name="package">The package, already read and hashed.</param>
    /// <param name="now">The instant to verify at; the host never reads a clock.</param>
    public ComponentVerification Verify(ComponentPackage package, long now)
    {
        ArgumentNullException.ThrowIfNull(package);
        var manifest = package.Manifest;

        // 1. The signing key is one a trusted certificate names.
        if (!_trust.TryFindCertificate(manifest.KeyId, out var certificate) || certificate is null)
        {
            return ComponentVerification.Refuse(
                ComponentRefusalCodes.UnknownKey,
                "the signing key " + manifest.KeyId + " is not named by any trusted publisher certificate.");
        }

        var body = certificate.Body;

        // 2. The certificate names the manifest's publisher.
        if (!string.Equals(body.PublisherId, manifest.PublisherId, StringComparison.Ordinal))
        {
            return ComponentVerification.Refuse(
                ComponentRefusalCodes.PublisherMismatch,
                "the manifest says it was published by '" + manifest.PublisherId + "', but key " + manifest.KeyId
                + " belongs to '" + body.PublisherId + "'.");
        }

        // 3. The certificate's window covers now.
        if (now < body.ValidFrom)
        {
            return ComponentVerification.Refuse(
                ComponentRefusalCodes.CertificateNotYetValid,
                "the certificate for '" + body.PublisherId + "' is not valid before "
                + body.ValidFrom.ToString(CultureInfo.InvariantCulture) + "; the host's time is "
                + now.ToString(CultureInfo.InvariantCulture) + ".");
        }

        if (now >= body.ValidTo)
        {
            return ComponentVerification.Refuse(
                ComponentRefusalCodes.CertificateExpired,
                "the certificate for '" + body.PublisherId + "' expired at "
                + body.ValidTo.ToString(CultureInfo.InvariantCulture) + "; the host's time is "
                + now.ToString(CultureInfo.InvariantCulture) + ".");
        }

        // 4. Revocation: the publisher, the key and the package digest. The date the manifest claims
        // for itself decides nothing (it is written by the publisher).
        var revocations = _trust.Revocations;
        if (revocations is not null)
        {
            var revoked = revocations.CheckPackageFacts(manifest.PublisherId, manifest.KeyId, manifest.SignedBytesSha256());
            if (revoked is not null)
            {
                return ComponentVerification.Refuse(revoked);
            }
        }

        // 5. The signature over the canonical manifest bytes.
        if (!Ed25519.Verify(body.PublicKey, manifest.SignedBytes(), package.Signature))
        {
            return ComponentVerification.Refuse(
                ComponentRefusalCodes.SignatureInvalid,
                "the signature does not verify over the canonical manifest of '" + manifest.ComponentId
                + "' with key " + manifest.KeyId + ".");
        }

        // 6. The contract version this host speaks.
        if (!manifest.ContractRange.Contains(_hostVersion))
        {
            var tooNew = manifest.ContractRange.Minimum > _hostVersion;
            return ComponentVerification.Refuse(
                tooNew ? ComponentRefusalCodes.ContractTooNew : ComponentRefusalCodes.ContractUnsupported,
                "'" + manifest.ComponentId + "' targets contract " + manifest.ContractRange.ToString()
                + " but this host speaks " + _hostVersion.ToString() + ".");
        }

        // 7. Every refusal above is final: the accepted facts are the ones the signature covered.
        return ComponentVerification.Accept(new VerifiedPackage(
            manifest,
            body.PublisherId,
            manifest.KeyId,
            manifest.SignedAt,
            _trust.HeldRevocationGeneration,
            manifest.SignedBytesSha256()));
    }
}
