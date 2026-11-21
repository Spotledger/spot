namespace Spotledger.Component.Host;

/// <summary>
/// Why a package was refused: a stable code a host may translate and a message a reviewer can act
/// on. The codes are the ones the README documents; the verification order that decides which one
/// is reported is documented on <see cref="ComponentVerifier"/>.
/// </summary>
/// <param name="Code">The stable refusal code, for example <c>component.signature.invalid</c>.</param>
/// <param name="Message">What was wrong, naming the file, key or version involved.</param>
public sealed record ComponentRefusal(string Code, string Message)
{
    /// <inheritdoc />
    public override string ToString() => Code + ": " + Message;
}

/// <summary>The outcome of verifying a package: the accepted facts, or a specific refusal.</summary>
public sealed class ComponentVerification
{
    private ComponentVerification(VerifiedPackage? accepted, ComponentRefusal? refusal)
    {
        Accepted = accepted;
        Refusal = refusal;
    }

    /// <summary>The accepted package facts; null when it was refused.</summary>
    public VerifiedPackage? Accepted { get; }

    /// <summary>The refusal; null when the package was accepted.</summary>
    public ComponentRefusal? Refusal { get; }

    /// <summary>True when the package may be loaded.</summary>
    public bool IsAccepted => Accepted is not null;

    /// <summary>An acceptance.</summary>
    public static ComponentVerification Accept(VerifiedPackage accepted) =>
        new(accepted ?? throw new ArgumentNullException(nameof(accepted)), null);

    /// <summary>A refusal.</summary>
    public static ComponentVerification Refuse(ComponentRefusal refusal) =>
        new(null, refusal ?? throw new ArgumentNullException(nameof(refusal)));

    /// <summary>A refusal with its code and message.</summary>
    public static ComponentVerification Refuse(string code, string message) => Refuse(new ComponentRefusal(code, message));
}

/// <summary>
/// What a successful verification establishes, and everything a later step (contract check, load)
/// needs: the manifest, the publisher and key the signature named, and the revocation generation
/// the check used.
/// </summary>
/// <param name="Manifest">The parsed manifest.</param>
/// <param name="PublisherId">The publisher the certificate names.</param>
/// <param name="KeyId">The signing key id, from the manifest.</param>
/// <param name="SignedAt">When the publisher CLAIMS to have signed (UTC seconds). The publisher wrote it; no decision rests on it.</param>
/// <param name="RevocationGeneration">The generation of the revocation list that was checked.</param>
/// <param name="SignedBytesSha256">The SHA-256 of the manifest's signed bytes, lower-case hex.</param>
public sealed record VerifiedPackage(
    ComponentManifest Manifest,
    string PublisherId,
    string KeyId,
    long SignedAt,
    ulong RevocationGeneration,
    string SignedBytesSha256);

/// <summary>
/// The stable refusal codes. A code names one rule, so a reviewer and a test can both point at it
/// without reading the message.
/// </summary>
public static class ComponentRefusalCodes
{
    /// <summary>The package has no <c>component.json</c>.</summary>
    public const string ManifestMissing = "component.package.manifest_missing";

    /// <summary>The manifest is not the documented JSON object.</summary>
    public const string ManifestInvalid = "component.manifest.invalid";

    /// <summary>The package has no <c>component.sig</c>.</summary>
    public const string SignatureMissing = "component.package.signature_missing";

    /// <summary>The signature file is not standard base64 of 64 bytes.</summary>
    public const string SignatureMalformed = "component.signature.malformed";

    /// <summary>A listed file is missing from the package.</summary>
    public const string FileMissing = "component.package.file_missing";

    /// <summary>A listed path is absolute, escapes the package or is not a file.</summary>
    public const string FilePathInvalid = "component.package.path_invalid";

    /// <summary>A file's SHA-256 is not the one the manifest lists: the package was modified.</summary>
    public const string HashMismatch = "component.package.hash_mismatch";

    /// <summary>The signing key is not in the trust store.</summary>
    public const string UnknownKey = "component.signature.unknown_key";

    /// <summary>The Ed25519 signature does not verify over the canonical manifest bytes.</summary>
    public const string SignatureInvalid = "component.signature.invalid";

    /// <summary>The certificate's publisher is not the manifest's <c>publisher_id</c>.</summary>
    public const string PublisherMismatch = "component.publisher.mismatch";

    /// <summary>The publisher is revoked at the signature's time.</summary>
    public const string PublisherRevoked = "component.publisher.revoked";

    /// <summary>The signing key is revoked at the signature's time.</summary>
    public const string KeyRevoked = "component.key.revoked";

    /// <summary>The signing key was compromised: every signature it made is refused.</summary>
    public const string KeyCompromised = "component.key.compromised";

    /// <summary>The package's digest is on the revocation list.</summary>
    public const string PackageRevoked = "component.package.revoked";

    /// <summary>The certificate is expired at the host's time.</summary>
    public const string CertificateExpired = "component.certificate.expired";

    /// <summary>The certificate is not valid yet at the host's time.</summary>
    public const string CertificateNotYetValid = "component.certificate.not_yet_valid";

    /// <summary>The certificate was not issued by a trusted store root.</summary>
    public const string UnknownRoot = "component.certificate.unknown_root";

    /// <summary>The certificate's own signature does not verify under its root.</summary>
    public const string CertificateSignatureInvalid = "component.certificate.signature_invalid";

    /// <summary>The revocation list's signature does not verify under its root.</summary>
    public const string RevocationSignatureInvalid = "component.revocation.signature_invalid";

    /// <summary>The trust store has already seen a newer revocation generation.</summary>
    public const string StaleRevocationList = "component.revocation.stale";

    /// <summary>The component targets a later contract major than this host speaks.</summary>
    public const string ContractTooNew = "component.contract.too_new";

    /// <summary>The component's contract range does not contain this host's version.</summary>
    public const string ContractUnsupported = "component.contract.unsupported";

    /// <summary>The assembly references an assembly neither in the package nor shared by the host.</summary>
    public const string HostPrivateDependency = "component.dependency.host_private";

    /// <summary>The package ships no assembly that exposes a component descriptor.</summary>
    public const string DescriptorMissing = "component.entry.descriptor_missing";

    /// <summary>The descriptor the package exposes is not sound.</summary>
    public const string DescriptorInvalid = "component.entry.descriptor_invalid";

    /// <summary>The manifest's identity and the descriptor's disagree.</summary>
    public const string DescriptorMismatch = "component.entry.descriptor_mismatch";

    /// <summary>The assembly is not loadable as a component package.</summary>
    public const string AssemblyInvalid = "component.assembly.invalid";

    /// <summary>The revocation list's generation is not above the one this store already holds; a replacement at an equal generation is refused too.</summary>
    public const string GenerationNotNewer = "component.revocation.generation_not_newer";

    /// <summary>The store's revocation generation is lower than one this host already installed under.</summary>
    public const string GenerationRegressed = "component.revocation.generation_regressed";

    /// <summary>An older version than the one installed before was offered without an explicit, recorded allowance.</summary>
    public const string DowngradeRefused = "component.version.downgrade_refused";

    /// <summary>The component was disabled because a revocation list withdrew it; it creates no new instance.</summary>
    public const string RevokedDisabled = "component.revoked.disabled";

    /// <summary>The persisted install state could not be read, so downgrade and revocation rules cannot be enforced.</summary>
    public const string StateUnreadable = "component.state.unreadable";

    /// <summary>A revocation disabled a component but the disabled set could not be saved; installs stay refused until it is.</summary>
    public const string RevocationNotPersisted = "component.revocation.not_persisted";

    /// <summary>The process must restart: code that already ran in it was revoked, could not be unloaded, or the revocation could not be saved.</summary>
    public const string RestartRequired = "component.restart.required";

    /// <summary>The persisted install state could not be written.</summary>
    public const string StateUnwritable = "component.state.unwritable";

    /// <summary>A package file could not be read (permission, I/O error).</summary>
    public const string FileUnreadable = "component.package.file_unreadable";

    /// <summary>A package file ended before its reported length, or changed while it was read.</summary>
    public const string FileTruncated = "component.package.file_truncated";

    /// <summary>Two assemblies of one package share a simple name.</summary>
    public const string DuplicateAssembly = "component.package.duplicate_assembly";

    /// <summary>The package lists or contains more files than a package may.</summary>
    public const string TooManyFiles = "component.package.too_many_files";

    /// <summary>A package file, or the package as a whole, is larger than a package may be.</summary>
    public const string FileOversized = "component.package.file_oversized";

    /// <summary>A symbolic link or reparse point exists inside the package.</summary>
    public const string SymlinkRefused = "component.package.symlink_refused";

    /// <summary>Loading failed in a way no specific code covers; nothing was installed.</summary>
    public const string LoadFailed = "component.load.failed";

    /// <summary>The component's constructor exceeded its budget; it is not run.</summary>
    public const string ConstructorBudgetExceeded = "component.resource.constructor_budget";

    /// <summary>The component's update exceeded its budget; the update is reported, not retried.</summary>
    public const string UpdateBudgetExceeded = "component.resource.update_budget";

    /// <summary>The component threw; a placeholder is shown in its slot.</summary>
    public const string ComponentThrew = "component.runtime.threw";

    /// <summary>A prop in a view reference is not one the component declares, or has the wrong type.</summary>
    public const string PropsRefused = "component.props.refused";

    /// <summary>The requested component id is not installed.</summary>
    public const string ComponentUnknown = "component.unknown";

    /// <summary>The shell's set manifest is not where the shell expects it.</summary>
    public const string SetManifestMissing = "component.set.manifest_missing";

    /// <summary>The shell's set manifest is not the documented JSON object.</summary>
    public const string SetManifestInvalid = "component.set.manifest_invalid";

    /// <summary>The set manifest's bytes do not hash to the content address in its file name.</summary>
    public const string SetNameMismatch = "component.set.name_mismatch";

    /// <summary>A component the set manifest lists is not installed.</summary>
    public const string SetComponentMissing = "component.set.component_missing";

    /// <summary>A component is installed but the set manifest does not list it.</summary>
    public const string SetComponentUnexpected = "component.set.component_unexpected";

    /// <summary>An installed component's version is not the version the set manifest lists.</summary>
    public const string SetVersionMismatch = "component.set.version_mismatch";

    /// <summary>An installed component's signed-manifest digest is not the one the set manifest lists.</summary>
    public const string SetDigestMismatch = "component.set.digest_mismatch";
}
