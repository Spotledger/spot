using System.Reflection;
using System.Runtime.Loader;

namespace Spotledger.Component.Host;

/// <summary>
/// The collectible context a package's assemblies load into. It exists for <em>lifetime</em>: it
/// gives the package its own dependency/version space, and <see cref="AssemblyLoadContext.Unload"/>
/// drops its code when nothing refers to it any more, which is what makes an uninstall complete.
/// <para>
/// It is <strong>not a sandbox</strong>. Code loaded here runs in this process with this process's
/// privileges; it can read the file system, open sockets and exhaust memory exactly as the host can.
/// What decides whether a component runs is the signature over its manifest and the store's review,
/// never the load context.
/// </para>
/// <para>
/// Resolution has three answers and no fourth: an assembly the host shares (the contract) comes from
/// the host; an assembly the package ships comes from the package's bytes; anything else that is the
/// application's own code is refused and recorded in <see cref="RefusedAssemblies"/>. A platform
/// assembly (the BCL, Avalonia) resolves from the default context, which is the process's own copy.
/// </para>
/// </summary>
public sealed class ComponentLoadContext : AssemblyLoadContext
{
    private readonly Dictionary<string, Assembly> _shared;
    private readonly Dictionary<string, byte[]> _packageAssemblies;
    private readonly List<string> _refused = new();

    /// <summary>Creates a collectible context for one installed component.</summary>
    /// <param name="name">The context name, shown in diagnostics.</param>
    /// <param name="sharedAssemblies">The host's shared assemblies, by simple name.</param>
    /// <param name="packageAssemblies">The package's assembly bytes, by simple name.</param>
    public ComponentLoadContext(
        string name,
        IReadOnlyDictionary<string, Assembly> sharedAssemblies,
        IReadOnlyDictionary<string, byte[]> packageAssemblies)
        : base(name, isCollectible: true)
    {
        _shared = new Dictionary<string, Assembly>(sharedAssemblies, StringComparer.Ordinal);
        _packageAssemblies = new Dictionary<string, byte[]>(packageAssemblies, StringComparer.Ordinal);
    }

    /// <summary>The assembly names this context refused to resolve because they are the application's.</summary>
    public IReadOnlyList<string> RefusedAssemblies => _refused;

    /// <summary>The package's assembly bytes this context was given.</summary>
    public IReadOnlyDictionary<string, byte[]> PackageAssemblies => _packageAssemblies;

    /// <summary>
    /// Loads one of the package's assemblies from its bytes, once.
    /// <para>
    /// The loaded assembly is looked up in <see cref="AssemblyLoadContext.Assemblies"/> rather than in
    /// a field of this context, and that is deliberate: a context that holds one of its own assemblies
    /// in a field is never collected, so <see cref="AssemblyLoadContext.Unload"/> would leave the
    /// package's code in the process for good. A test proves the collection with a weak reference.
    /// </para>
    /// </summary>
    /// <param name="simpleName">The assembly's simple name.</param>
    public Assembly LoadPackageAssembly(string simpleName)
    {
        ArgumentException.ThrowIfNullOrEmpty(simpleName);
        foreach (var loaded in Assemblies)
        {
            if (string.Equals(loaded.GetName().Name, simpleName, StringComparison.Ordinal))
            {
                return loaded;
            }
        }

        if (!_packageAssemblies.TryGetValue(simpleName, out var bytes))
        {
            throw new FileNotFoundException(
                "The package does not ship an assembly named '" + simpleName + "'.", simpleName);
        }

        using var stream = new MemoryStream(bytes, writable: false);
        return LoadFromStream(stream);
    }

    /// <inheritdoc />
    protected override Assembly? Load(AssemblyName assemblyName)
    {
        ArgumentNullException.ThrowIfNull(assemblyName);
        var name = assemblyName.Name;
        if (string.IsNullOrEmpty(name))
        {
            return null;
        }

        if (_shared.TryGetValue(name, out var shared))
        {
            return shared;
        }

        if (_packageAssemblies.ContainsKey(name))
        {
            return LoadPackageAssembly(name);
        }

        if (ComponentDependencies.IsHostPrivate(name))
        {
            _refused.Add(name);
            return null;
        }

        // The platform (the BCL, Avalonia): the runtime's default context answers, which is the
        // process's own copy and never the package's.
        return null;
    }
}
