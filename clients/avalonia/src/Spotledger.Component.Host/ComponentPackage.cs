using System.Globalization;
using System.Security.Cryptography;

namespace Spotledger.Component.Host;

/// <summary>
/// The path rules of a package. A manifest names files relative to the package root with forward
/// slashes; an absolute path, a drive, a backslash or a <c>..</c> segment is refused, so a package
/// can never make the host read outside its own directory.
/// </summary>
public static class ComponentPackagePaths
{
    /// <summary>The manifest's file name.</summary>
    public const string ManifestFileName = "component.json";

    /// <summary>The detached signature's file name: standard base64 of the 64 signature bytes.</summary>
    public const string SignatureFileName = "component.sig";

    /// <summary>True when <paramref name="path"/> is a safe package-relative path.</summary>
    /// <param name="path">The path as the manifest wrote it.</param>
    /// <param name="problem">Why it is refused; empty when it is safe.</param>
    public static bool IsSafeRelativePath(string? path, out string problem)
    {
        problem = string.Empty;
        if (string.IsNullOrEmpty(path))
        {
            problem = "a file path is not empty.";
            return false;
        }

        var shown = ComponentRefusalText.SafePath(path);
        if (path.Length > 512)
        {
            problem = "'" + shown + "' is longer than 512 characters.";
            return false;
        }

        if (path.Contains('\\', StringComparison.Ordinal))
        {
            problem = "'" + shown + "' uses a backslash; package paths use '/'.";
            return false;
        }

        if (path[0] == '/')
        {
            problem = "'" + shown + "' is absolute; package paths are relative.";
            return false;
        }

        if (path.Contains(':', StringComparison.Ordinal))
        {
            problem = "'" + shown + "' names a drive or a stream.";
            return false;
        }

        foreach (var segment in path.Split('/'))
        {
            if (segment.Length == 0 || segment == ".")
            {
                problem = "'" + shown + "' has an empty or '.' segment.";
                return false;
            }

            if (segment == "..")
            {
                problem = "'" + shown + "' leaves the package.";
                return false;
            }

            if (segment.Length > 128)
            {
                problem = "'" + shown + "' has a segment longer than 128 characters.";
                return false;
            }
        }

        return true;
    }

    /// <summary>The absolute path of a package-relative path, after checking it is safe.</summary>
    /// <param name="packageRoot">The package's directory.</param>
    /// <param name="relativePath">The package-relative path.</param>
    public static string Resolve(string packageRoot, string relativePath)
    {
        ArgumentException.ThrowIfNullOrEmpty(packageRoot);
        if (!IsSafeRelativePath(relativePath, out var problem))
        {
            throw new ArgumentException(problem, nameof(relativePath));
        }

        var combined = Path.GetFullPath(Path.Combine(packageRoot, relativePath.Replace('/', Path.DirectorySeparatorChar)));
        var root = Path.GetFullPath(packageRoot);
        var rootWithSeparator = root.EndsWith(Path.DirectorySeparatorChar) ? root : root + Path.DirectorySeparatorChar;
        if (!combined.StartsWith(rootWithSeparator, StringComparison.Ordinal))
        {
            throw new ArgumentException("'" + ComponentRefusalText.SafePath(relativePath) + "' leaves the package.", nameof(relativePath));
        }

        return combined;
    }
}

/// <summary>One file of a package as it was read: its path, the manifest's hash and its bytes.</summary>
/// <param name="Path">The package-relative path.</param>
/// <param name="Sha256">The SHA-256 the manifest lists, lower-case hex.</param>
/// <param name="Bytes">The bytes that hashed to it.</param>
public sealed record PackageFile(string Path, string Sha256, byte[] Bytes);

/// <summary>
/// A package as the host reads it, before anything is loaded: the parsed manifest, the detached
/// signature and the bytes of every listed file, each already checked against its manifest hash.
/// Reading is the only step that touches the file system; verification and loading work from these
/// bytes, so a file cannot change between the hash check and the load.
/// </summary>
public sealed class ComponentPackage
{
    private readonly Dictionary<string, PackageFile> _files;

    private ComponentPackage(string root, ComponentManifest manifest, byte[] signature, IReadOnlyList<PackageFile> files)
    {
        Root = root;
        Manifest = manifest;
        Signature = signature;
        Files = files;
        _files = files.ToDictionary(static f => f.Path, StringComparer.Ordinal);
    }

    /// <summary>The package's directory.</summary>
    public string Root { get; }

    /// <summary>The parsed manifest.</summary>
    public ComponentManifest Manifest { get; }

    /// <summary>The 64 signature bytes, exactly as <c>component.sig</c> carried them.</summary>
    public byte[] Signature { get; }

    /// <summary>Every listed file, assemblies then assets, sorted by path within each list.</summary>
    public IReadOnlyList<PackageFile> Files { get; }

    /// <summary>The bytes of the entry assembly, which the loader reads into its own context.</summary>
    public PackageFile EntryAssembly => _files[Manifest.EntryAssembly];

    /// <summary>The bytes of a listed file.</summary>
    /// <param name="path">The package-relative path.</param>
    public bool TryGetFile(string path, out PackageFile? file) => _files.TryGetValue(path, out file);

    /// <summary>The SHA-256 of a file's bytes, lower-case hex: what a manifest lists.</summary>
    public static string HashFile(ReadOnlySpan<byte> bytes) => Convert.ToHexStringLower(SHA256.HashData(bytes));

    /// <summary>The most files a manifest may list.</summary>
    public const int MaxFiles = 256;

    /// <summary>The most bytes one package file may have.</summary>
    public const long MaxFileBytes = 64L * 1024 * 1024;

    /// <summary>The most bytes the listed files may have together.</summary>
    public const long MaxTotalBytes = 256L * 1024 * 1024;

    /// <summary>The most bytes the manifest may have.</summary>
    public const long MaxManifestBytes = 1024L * 1024;

    /// <summary>The most directory entries a package directory may hold (files, folders and anything else).</summary>
    public const int MaxEntries = 4096;

    private const long MaxSignatureBytes = 4096;

    /// <summary>
    /// Reads a package directory: the manifest, the signature, and the bytes of every listed file,
    /// each refused with a specific code and a plain-text message — never an exception. A symbolic link
    /// or reparse point anywhere inside the directory is refused (<c>symlink_refused</c>), a file that
    /// is missing, unreadable, truncated or too large is refused, and so is a package that lists too
    /// many files or two assemblies with one simple name. A hash mismatch names the file, never a hash.
    /// </summary>
    /// <remarks>
    /// Review aid, not a sandbox: the link check and the read are two steps, so a writer who can
    /// change the package directory while it is read can still race them. The directory is expected
    /// to be one only the installer can write.
    /// </remarks>
    /// <param name="directory">The package root.</param>
    public static ComponentPackageRead Read(string directory)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(directory);
        try
        {
            return ReadChecked(directory);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or System.Security.SecurityException or NotSupportedException or ArgumentException)
        {
            return ComponentPackageRead.Refuse(
                ComponentRefusalCodes.FileUnreadable,
                "the package could not be read: a file is unreadable, or was removed or changed while it was read.");
        }
    }

    private static ComponentPackageRead ReadChecked(string directory)
    {
        if (!Directory.Exists(directory))
        {
            return ComponentPackageRead.Refuse(
                ComponentRefusalCodes.FileMissing,
                "the package directory does not exist.");
        }

        var scan = ScanForLinks(directory);
        if (scan is not null)
        {
            return ComponentPackageRead.Refuse(scan);
        }

        var manifestRead = ReadBounded(directory, ComponentPackagePaths.ManifestFileName, MaxManifestBytes);
        if (manifestRead.Refusal is { } manifestRefusal)
        {
            return ComponentPackageRead.Refuse(manifestRefusal.Code == ComponentRefusalCodes.FileMissing
                ? new ComponentRefusal(
                    ComponentRefusalCodes.ManifestMissing,
                    "'" + ComponentPackagePaths.ManifestFileName + "' is missing from the package.")
                : manifestRefusal);
        }

        var manifestText = System.Text.Encoding.UTF8.GetString(manifestRead.Bytes!);
        if (!ComponentManifest.TryParse(manifestText, out var manifest, out var problems) || manifest is null)
        {
            return ComponentPackageRead.Refuse(
                ComponentRefusalCodes.ManifestInvalid,
                string.Join(" ", problems));
        }

        var signatureRead = ReadBounded(directory, ComponentPackagePaths.SignatureFileName, MaxSignatureBytes);
        if (signatureRead.Refusal is { } signatureRefusal)
        {
            return ComponentPackageRead.Refuse(signatureRefusal.Code == ComponentRefusalCodes.FileMissing
                ? new ComponentRefusal(
                    ComponentRefusalCodes.SignatureMissing,
                    "'" + ComponentPackagePaths.SignatureFileName + "' is missing: an unsigned package is refused.")
                : signatureRefusal);
        }

        if (!Ed25519.TryFromBase64(System.Text.Encoding.UTF8.GetString(signatureRead.Bytes!), out var signature)
            || signature.Length != Ed25519.SignatureBytes)
        {
            return ComponentPackageRead.Refuse(
                ComponentRefusalCodes.SignatureMalformed,
                "'" + ComponentPackagePaths.SignatureFileName + "' is not standard base64 of 64 bytes.");
        }

        var listedFiles = manifest.Files.ToArray();
        if (listedFiles.Length > MaxFiles)
        {
            return ComponentPackageRead.Refuse(
                ComponentRefusalCodes.TooManyFiles,
                "the manifest lists " + listedFiles.Length.ToString(CultureInfo.InvariantCulture)
                + " files; a package may list at most " + MaxFiles.ToString(CultureInfo.InvariantCulture) + ".");
        }

        var duplicate = manifest.Assemblies
            .GroupBy(static a => Path.GetFileNameWithoutExtension(a.Path), StringComparer.OrdinalIgnoreCase)
            .FirstOrDefault(static g => g.Count() > 1);
        if (duplicate is not null)
        {
            return ComponentPackageRead.Refuse(
                ComponentRefusalCodes.DuplicateAssembly,
                "two assemblies of the package share the simple name '" + duplicate.Key
                + "'; each assembly of a package has a name of its own.");
        }

        var files = new List<PackageFile>(listedFiles.Length);
        long total = 0;
        var index = -1;
        foreach (var listed in listedFiles)
        {
            index++;
            var label = ComponentRefusalText.FileRef(index, listed.Path);
            try
            {
                _ = ComponentPackagePaths.Resolve(directory, listed.Path);
            }
            catch (ArgumentException error)
            {
                return ComponentPackageRead.Refuse(ComponentRefusalCodes.FilePathInvalid, error.Message);
            }

            var read = ReadBounded(directory, listed.Path, MaxFileBytes, label);
            if (read.Refusal is { } fileRefusal)
            {
                return ComponentPackageRead.Refuse(fileRefusal.Code == ComponentRefusalCodes.FileMissing
                    ? new ComponentRefusal(
                        ComponentRefusalCodes.FileMissing,
                        "the manifest lists " + label + " but the package does not contain it.")
                    : fileRefusal);
            }

            var bytes = read.Bytes!;
            total += bytes.LongLength;
            if (total > MaxTotalBytes)
            {
                return ComponentPackageRead.Refuse(
                    ComponentRefusalCodes.FileOversized,
                    "the listed files together are larger than the "
                    + (MaxTotalBytes / (1024 * 1024)).ToString(CultureInfo.InvariantCulture) + " MiB a package may have.");
            }

            if (!string.Equals(HashFile(bytes), listed.Sha256, StringComparison.Ordinal))
            {
                // Neither hash is printed: the message says which file, so the package can be fixed,
                // and nothing about the bytes of the file that was found.
                return ComponentPackageRead.Refuse(
                    ComponentRefusalCodes.HashMismatch,
                    "package file hash mismatch: " + label + ".");
            }

            files.Add(new PackageFile(listed.Path, listed.Sha256, bytes));
        }

        return ComponentPackageRead.Accept(new ComponentPackage(directory, manifest, signature, files));
    }

    // Any symbolic link or reparse point inside the directory is refused, listed or not: a link can
    // point outside the package, and a package has no use for one. The entry count is capped so a
    // directory cycle or a flood of entries ends here.
    private static ComponentRefusal? ScanForLinks(string directory)
    {
        var options = new EnumerationOptions
        {
            RecurseSubdirectories = true,
            AttributesToSkip = 0,
            IgnoreInaccessible = false,
            ReturnSpecialDirectories = false,
        };
        var count = 0;
        foreach (var entry in new DirectoryInfo(directory).EnumerateFileSystemInfos("*", options))
        {
            if (entry.LinkTarget is not null || (entry.Attributes & FileAttributes.ReparsePoint) != 0)
            {
                return new ComponentRefusal(
                    ComponentRefusalCodes.SymlinkRefused,
                    "the package contains a symbolic link or reparse point at '"
                    + ComponentRefusalText.SafePath(Path.GetRelativePath(directory, entry.FullName)) + "'; links are refused.");
            }

            if (++count > MaxEntries)
            {
                return new ComponentRefusal(
                    ComponentRefusalCodes.TooManyFiles,
                    "the package directory holds more than " + MaxEntries.ToString(CultureInfo.InvariantCulture) + " entries.");
            }
        }

        return null;
    }

    private static BoundedRead ReadBounded(string directory, string relativePath, long maxBytes, string? label = null)
    {
        var shown = label ?? "'" + ComponentRefusalText.SafePath(relativePath) + "'";
        string absolute;
        try
        {
            absolute = ComponentPackagePaths.Resolve(directory, relativePath);
        }
        catch (ArgumentException error)
        {
            return BoundedRead.Refuse(ComponentRefusalCodes.FilePathInvalid, error.Message);
        }

        var info = new FileInfo(absolute);
        if (!info.Exists)
        {
            return BoundedRead.Refuse(ComponentRefusalCodes.FileMissing, shown + " does not exist.");
        }

        // The link check again, right before the open: a file replaced by a link since the scan.
        if (info.LinkTarget is not null || info.ResolveLinkTarget(returnFinalTarget: true) is not null)
        {
            return BoundedRead.Refuse(
                ComponentRefusalCodes.SymlinkRefused,
                shown + " is a symbolic link; links are refused.");
        }

        try
        {
            using var stream = new FileStream(absolute, FileMode.Open, FileAccess.Read, FileShare.Read);
            var length = stream.Length;
            if (length > maxBytes)
            {
                return BoundedRead.Refuse(
                    ComponentRefusalCodes.FileOversized,
                    shown + " is larger than the " + maxBytes.ToString(CultureInfo.InvariantCulture)
                    + " bytes a file of this kind may have.");
            }

            var bytes = new byte[length];
            var filled = 0;
            while (filled < bytes.Length)
            {
                var n = stream.Read(bytes, filled, bytes.Length - filled);
                if (n == 0)
                {
                    return BoundedRead.Refuse(
                        ComponentRefusalCodes.FileTruncated,
                        shown + " ended before its reported length: it was truncated or changed while it was read.");
                }

                filled += n;
            }

            if (stream.Length != length || stream.ReadByte() >= 0)
            {
                return BoundedRead.Refuse(
                    ComponentRefusalCodes.FileTruncated,
                    shown + " changed while it was read.");
            }

            return BoundedRead.Accept(bytes);
        }
        catch (FileNotFoundException)
        {
            return BoundedRead.Refuse(ComponentRefusalCodes.FileMissing, shown + " does not exist.");
        }
        catch (DirectoryNotFoundException)
        {
            return BoundedRead.Refuse(ComponentRefusalCodes.FileMissing, shown + " does not exist.");
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            return BoundedRead.Refuse(
                ComponentRefusalCodes.FileUnreadable,
                shown + " could not be read (the file is unreadable or in use).");
        }
    }

    private readonly record struct BoundedRead(byte[]? Bytes, ComponentRefusal? Refusal)
    {
        public static BoundedRead Accept(byte[] bytes) => new(bytes, null);

        public static BoundedRead Refuse(string code, string message) => new(null, new ComponentRefusal(code, message));
    }

    /// <inheritdoc />
    public override string ToString() =>
        Manifest.ComponentId + " with "
        + Files.Count.ToString(CultureInfo.InvariantCulture) + " file(s)";
}

/// <summary>The outcome of reading a package: the package, or the specific refusal.</summary>
public sealed class ComponentPackageRead
{
    private ComponentPackageRead(ComponentPackage? package, ComponentRefusal? refusal)
    {
        Package = package;
        Refusal = refusal;
    }

    /// <summary>The package; null when it was refused.</summary>
    public ComponentPackage? Package { get; }

    /// <summary>The refusal; null when the package was read.</summary>
    public ComponentRefusal? Refusal { get; }

    /// <summary>True when the package was read.</summary>
    public bool IsAccepted => Package is not null;

    /// <summary>An accepted read.</summary>
    public static ComponentPackageRead Accept(ComponentPackage package) =>
        new(package ?? throw new ArgumentNullException(nameof(package)), null);

    /// <summary>A refusal.</summary>
    public static ComponentPackageRead Refuse(string code, string message) =>
        new(null, new ComponentRefusal(code, message));

    /// <summary>A refusal.</summary>
    public static ComponentPackageRead Refuse(ComponentRefusal refusal) =>
        new(null, refusal ?? throw new ArgumentNullException(nameof(refusal)));
}
