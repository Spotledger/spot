using System.Reflection.Metadata;
using System.Reflection.PortableExecutable;

namespace Spotledger.Component.Host;

/// <summary>
/// Which assemblies a package may depend on. The rule is narrow on purpose: a component resolves the
/// assemblies it ships, the contract assemblies the host shares with it, and the platform
/// (the BCL and Avalonia, which the process already runs). Every other <c>Spotledger.*</c> assembly
/// is the application's own code — its view models, its services, its API client — and a package may
/// not reach it, because a component is supposed to render one field through the contract, not to
/// become part of the app.
/// <para>
/// This is a dependency rule, not a sandbox: an accepted component runs in this process with its
/// privileges. Review and the signature decide whether code runs at all.
/// </para>
/// </summary>
public static class ComponentDependencies
{
    /// <summary>The assemblies the host shares with every package.</summary>
    public static IReadOnlyCollection<string> SharedAssemblyNames { get; } = new[]
    {
        typeof(Contract.IComponent).Assembly.GetName().Name ?? "Spotledger.Component.Contract",
    };

    /// <summary>True when <paramref name="assemblyName"/> is the application's private code.</summary>
    /// <param name="assemblyName">A simple assembly name.</param>
    public static bool IsHostPrivate(string assemblyName)
    {
        if (string.IsNullOrEmpty(assemblyName))
        {
            return false;
        }

        // The prefix is matched ignoring case because the runtime binds simple names ignoring case:
        // a hand-made reference to 'SPOTLEDGER.Client.Core' would otherwise slip past this rule and be
        // answered by the application's own copy. The shared names stay an exact (ordinal) match, so a
        // differently-cased spelling of the contract is refused rather than waved through.
        return assemblyName.StartsWith("Spotledger.", StringComparison.OrdinalIgnoreCase)
            && !SharedAssemblyNames.Contains(assemblyName, StringComparer.Ordinal);
    }

    /// <summary>
    /// Every assembly reference of <paramref name="assemblyBytes"/>, from the metadata table — read,
    /// never executed.
    /// </summary>
    /// <param name="assemblyBytes">The PE bytes.</param>
    /// <param name="references">The referenced simple names; empty when the bytes are not a PE file.</param>
    public static bool TryReadReferences(byte[] assemblyBytes, out IReadOnlyList<string> references)
    {
        references = Array.Empty<string>();
        try
        {
            using var stream = new MemoryStream(assemblyBytes, writable: false);
            using var reader = new PEReader(stream);
            if (!reader.HasMetadata)
            {
                return false;
            }

            var metadata = reader.GetMetadataReader();
            references = metadata.AssemblyReferences
                .Select(handle => metadata.GetString(metadata.GetAssemblyReference(handle).Name))
                .ToArray();
            return true;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            // Truncated or hostile bytes surface as BadImageFormat, InvalidOperation, Argument or
            // overflow errors depending on where the parser stops; all of them mean "not an assembly".
            return false;
        }
    }

    /// <summary>
    /// The first reference in <paramref name="assemblyBytes"/> that the package may not resolve: an
    /// application-private assembly. A reference that resolves to neither the package, the shared
    /// contract assemblies nor the platform fails at load time with the runtime's own error, and the
    /// load context records it; the application-private case is refused here, before any load.
    /// </summary>
    /// <param name="assemblyBytes">The PE bytes.</param>
    public static ComponentRefusal? Check(byte[] assemblyBytes)
    {
        ArgumentNullException.ThrowIfNull(assemblyBytes);
        if (!TryReadReferences(assemblyBytes, out var references))
        {
            return new ComponentRefusal(
                ComponentRefusalCodes.AssemblyInvalid,
                "the assembly is not a loadable .NET assembly (no managed metadata).");
        }

        foreach (var reference in references)
        {
            if (IsHostPrivate(reference))
            {
                return new ComponentRefusal(
                    ComponentRefusalCodes.HostPrivateDependency,
                    "the assembly references '" + ComponentRefusalText.SafePath(reference)
                    + "', which is the application's own code: a package may use the platform, the contract ("
                    + string.Join(", ", SharedAssemblyNames) + ") and the assemblies it ships, nothing else.");
            }
        }

        return null;
    }

    /// <summary>The simple assembly names a package ships, from the file names of its assemblies.</summary>
    /// <param name="paths">The package-relative assembly paths.</param>
    public static IReadOnlyCollection<string> NamesOf(IEnumerable<string> paths)
    {
        ArgumentNullException.ThrowIfNull(paths);
        return paths
            .Select(static path => Path.GetFileNameWithoutExtension(path))
            .Where(static name => !string.IsNullOrEmpty(name))
            .ToArray();
    }
}
