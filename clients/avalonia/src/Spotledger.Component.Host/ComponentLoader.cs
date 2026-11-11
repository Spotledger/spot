using System.Reflection;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>The outcome of a load: the component, or the specific refusal that stopped it.</summary>
public sealed class ComponentLoadResult
{
    private ComponentLoadResult(LoadedComponent? component, ComponentRefusal? refusal)
    {
        Component = component;
        Refusal = refusal;
    }

    /// <summary>The loaded component; null when it was refused.</summary>
    public LoadedComponent? Component { get; }

    /// <summary>The refusal; null when the component loaded.</summary>
    public ComponentRefusal? Refusal { get; }

    /// <summary>True when the component loaded and is installed.</summary>
    public bool IsLoaded => Component is not null;

    /// <summary>A load.</summary>
    public static ComponentLoadResult Loaded(LoadedComponent component) =>
        new(component ?? throw new ArgumentNullException(nameof(component)), null);

    /// <summary>A refusal.</summary>
    public static ComponentLoadResult Refuse(ComponentRefusal refusal) =>
        new(null, refusal ?? throw new ArgumentNullException(nameof(refusal)));
}

/// <summary>
/// Loads a verified package into a collectible context and registers it. The order is the promise:
/// everything is checked first — signature, file hashes (by <see cref="ComponentPackage.Read"/>),
/// revocation, the contract range, then each assembly's references — and only then is an
/// <see cref="ComponentLoadContext"/> created and the entry assembly read.
/// <para>
/// The component's entry point is a public static member of type
/// <see cref="ComponentDescriptor"/> in the entry assembly: <c>Instance</c> is preferred (the T1
/// sample's name), <c>Descriptor</c> next. The descriptor's id, version, publisher and contract
/// range must equal the manifest's, so the fields the signature covered are the fields that run.
/// </para>
/// </summary>
public sealed class ComponentLoader
{
    private readonly ComponentVerifier _verifier;
    private readonly ComponentCatalogue _catalogue;
    private readonly IComponentHostLog _log;
    private readonly IReadOnlyDictionary<string, Assembly> _shared;

    /// <summary>Creates a loader over a trust store and the catalogue it installs into.</summary>
    /// <param name="trust">The trust material the store registry supplied.</param>
    /// <param name="catalogue">The catalogue an accepted component joins.</param>
    /// <param name="log">Where refusals and loads are recorded.</param>
    /// <param name="hostVersion">The contract version this host speaks; the contract's when null.</param>
    public ComponentLoader(
        ComponentTrustStore trust,
        ComponentCatalogue catalogue,
        IComponentHostLog? log = null,
        ContractVersion? hostVersion = null)
    {
        ArgumentNullException.ThrowIfNull(trust);
        _catalogue = catalogue ?? throw new ArgumentNullException(nameof(catalogue));
        _log = log ?? NullComponentHostLog.Instance;
        _verifier = new ComponentVerifier(trust, hostVersion);
        _shared = new Dictionary<string, Assembly>(StringComparer.Ordinal)
        {
            [typeof(IComponent).Assembly.GetName().Name ?? "Spotledger.Component.Contract"] = typeof(IComponent).Assembly,
        };
    }

    /// <summary>Reads and loads a package directory at <paramref name="now"/> (UTC seconds).</summary>
    /// <param name="directory">The package root.</param>
    /// <param name="now">The instant to verify at.</param>
    public ComponentLoadResult Load(string directory, long now) => Load(directory, now, null);

    /// <summary>
    /// Reads and loads a package directory, asking <paramref name="admission"/> after verification and
    /// before any code is loaded whether the host will take this verified package (the installation
    /// service uses it for the downgrade and revocation-memory rules). Never throws: every failure is
    /// a refusal.
    /// </summary>
    /// <param name="directory">The package root.</param>
    /// <param name="now">The instant to verify at.</param>
    /// <param name="admission">Returns a refusal to stop the load, or null.</param>
    public ComponentLoadResult Load(string directory, long now, Func<VerifiedPackage, ComponentRefusal?>? admission)
    {
        var read = ComponentPackage.Read(directory);
        if (!read.IsAccepted || read.Package is null)
        {
            return Refuse(read.Refusal ?? new ComponentRefusal(
                ComponentRefusalCodes.ManifestInvalid,
                "the package could not be read."));
        }

        return Load(read.Package, now, admission);
    }

    /// <summary>Verifies and loads an already-read package at <paramref name="now"/> (UTC seconds).</summary>
    /// <param name="package">The package.</param>
    /// <param name="now">The instant to verify at.</param>
    public ComponentLoadResult Load(ComponentPackage package, long now) => Load(package, now, null);

    /// <summary>Verifies and loads an already-read package; see <see cref="Load(string, long, Func{VerifiedPackage, ComponentRefusal?})"/>.</summary>
    /// <param name="package">The package.</param>
    /// <param name="now">The instant to verify at.</param>
    /// <param name="admission">Returns a refusal to stop the load, or null.</param>
    public ComponentLoadResult Load(ComponentPackage package, long now, Func<VerifiedPackage, ComponentRefusal?>? admission)
    {
        ArgumentNullException.ThrowIfNull(package);
        try
        {
            return LoadCore(package, now, admission);
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            // The last net: anything the specific checks missed is still a refusal, never a throw.
            return Refuse(new ComponentRefusal(
                ComponentRefusalCodes.LoadFailed,
                "an unexpected problem stopped the load of '" + package.Manifest.ComponentId
                + "' (" + error.GetType().Name + "); nothing was installed."));
        }
    }

    private ComponentLoadResult LoadCore(ComponentPackage package, long now, Func<VerifiedPackage, ComponentRefusal?>? admission)
    {
        var verification = _verifier.Verify(package, now);
        if (!verification.IsAccepted || verification.Accepted is null)
        {
            return Refuse(verification.Refusal ?? new ComponentRefusal(
                ComponentRefusalCodes.SignatureInvalid,
                "the package did not verify."));
        }

        var admitted = admission?.Invoke(verification.Accepted);
        if (admitted is not null)
        {
            return Refuse(admitted);
        }

        // Every assembly is inspected before any context exists: an application-private reference is
        // refused here, with the name of the assembly it wanted.
        var assemblyIndex = -1;
        foreach (var listed in package.Manifest.Assemblies)
        {
            assemblyIndex++;
            var label = ComponentRefusalText.FileRef(assemblyIndex, listed.Path);
            if (!package.TryGetFile(listed.Path, out var file) || file is null)
            {
                return Refuse(new ComponentRefusal(
                    ComponentRefusalCodes.FileMissing,
                    label + " was listed but not read."));
            }

            var dependencyRefusal = ComponentDependencies.Check(file.Bytes);
            if (dependencyRefusal is not null)
            {
                return Refuse(new ComponentRefusal(
                    dependencyRefusal.Code,
                    label + " " + dependencyRefusal.Message));
            }
        }

        // Verified. Only now does the package get a context.
        var context = new ComponentLoadContext(
            package.Manifest.ComponentId + "@" + package.Manifest.Version.ToString(),
            _shared,
            package.Manifest.Assemblies.ToDictionary(
                static a => Path.GetFileNameWithoutExtension(a.Path),
                a => package.EntryAssembly.Path == a.Path ? package.EntryAssembly.Bytes : FileOf(package, a.Path),
                StringComparer.Ordinal));

        ComponentLoadResult loaded;
        try
        {
            loaded = LoadInto(package, verification.Accepted, context);
        }
        catch
        {
            context.Unload();
            throw;
        }

        if (!loaded.IsLoaded)
        {
            context.Unload();
            return loaded;
        }

        var registrationRefusal = _catalogue.Add(loaded.Component!);
        if (registrationRefusal is not null)
        {
            loaded.Component!.Dispose();
            return Refuse(registrationRefusal);
        }

        _log.Loaded(loaded.Component!);
        return loaded;
    }

    private ComponentLoadResult LoadInto(ComponentPackage package, VerifiedPackage verified, ComponentLoadContext context)
    {
        var entryName = Path.GetFileNameWithoutExtension(package.Manifest.EntryAssembly);
        Assembly assembly;
        try
        {
            assembly = context.LoadPackageAssembly(entryName);
        }
        catch (BadImageFormatException error)
        {
            return Refuse(new ComponentRefusal(
                ComponentRefusalCodes.AssemblyInvalid,
                "the entry assembly " + ComponentRefusalText.FileRef(EntryIndex(package), package.Manifest.EntryAssembly)
                + " is not a loadable assembly: " + ComponentRefusalText.SafePath(error.Message)));
        }
        catch (FileLoadException error)
        {
            return Refuse(new ComponentRefusal(
                ComponentRefusalCodes.AssemblyInvalid,
                "the entry assembly " + ComponentRefusalText.FileRef(EntryIndex(package), package.Manifest.EntryAssembly)
                + " could not be loaded: " + ComponentRefusalText.SafePath(error.Message)));
        }
        catch (Exception error) when (error is IOException or ArgumentException or InvalidOperationException or BadImageFormatException)
        {
            return Refuse(new ComponentRefusal(
                ComponentRefusalCodes.AssemblyInvalid,
                "the entry assembly " + ComponentRefusalText.FileRef(EntryIndex(package), package.Manifest.EntryAssembly)
                + " is not a loadable assembly."));
        }

        if (!TryFindDescriptor(assembly, out var descriptor, out var discoveryRefusal))
        {
            return Refuse(discoveryRefusal!);
        }

        var mismatch = CompareManifestWithDescriptor(package.Manifest, descriptor!);
        if (mismatch is not null)
        {
            return Refuse(mismatch);
        }

        var problems = descriptor!.Validate();
        if (problems.Count > 0)
        {
            return Refuse(new ComponentRefusal(ComponentRefusalCodes.DescriptorInvalid, string.Join(" ", problems)));
        }

        if (!ComponentContract.TryAccept(descriptor.ContractRange, out var contractCode, out var contractMessage))
        {
            return Refuse(new ComponentRefusal(contractCode ?? ComponentRefusalCodes.ContractUnsupported, contractMessage ?? string.Empty));
        }

        return ComponentLoadResult.Loaded(new LoadedComponent(package, verified, descriptor, context));
    }

    private ComponentLoadResult Refuse(ComponentRefusal refusal)
    {
        _log.Refused(refusal);
        return ComponentLoadResult.Refuse(refusal);
    }

    private static int EntryIndex(ComponentPackage package)
    {
        for (var i = 0; i < package.Files.Count; i++)
        {
            if (string.Equals(package.Files[i].Path, package.Manifest.EntryAssembly, StringComparison.Ordinal))
            {
                return i;
            }
        }

        return -1;
    }

    private static byte[] FileOf(ComponentPackage package, string path) =>
        package.TryGetFile(path, out var file) && file is not null
            ? file.Bytes
            : throw new InvalidOperationException("'" + ComponentRefusalText.SafePath(path) + "' was listed but not read.");

    private static ComponentRefusal? CompareManifestWithDescriptor(ComponentManifest manifest, ComponentDescriptor descriptor)
    {
        var differences = new List<string>();
        if (!string.Equals(manifest.ComponentId, descriptor.Id, StringComparison.Ordinal))
        {
            differences.Add("id " + descriptor.Id + " is not " + manifest.ComponentId);
        }

        if (manifest.Version != descriptor.Version)
        {
            differences.Add("version " + descriptor.Version.ToString() + " is not " + manifest.Version.ToString());
        }

        if (!string.Equals(manifest.PublisherId, descriptor.Publisher, StringComparison.Ordinal))
        {
            differences.Add("publisher " + descriptor.Publisher + " is not " + manifest.PublisherId);
        }

        if (descriptor.ContractRange.Minimum != manifest.ContractRange.Minimum
            || descriptor.ContractRange.MaximumExclusive != manifest.ContractRange.MaximumExclusive)
        {
            differences.Add("contract range " + descriptor.ContractRange.ToString() + " is not "
                + manifest.ContractRange.ToString());
        }

        return differences.Count == 0
            ? null
            : new ComponentRefusal(
                ComponentRefusalCodes.DescriptorMismatch,
                "the signed manifest and the component's descriptor disagree: " + string.Join("; ", differences) + ".");
    }

    private static bool TryFindDescriptor(
        Assembly assembly,
        out ComponentDescriptor? descriptor,
        out ComponentRefusal? refusal)
    {
        descriptor = null;
        refusal = null;
        Type[] types;
        try
        {
            types = assembly.GetTypes();
        }
        catch (ReflectionTypeLoadException error)
        {
            refusal = new ComponentRefusal(
                ComponentRefusalCodes.AssemblyInvalid,
                "the entry assembly's types could not be read: "
                + ComponentRefusalText.SafePath(string.Join("; ", error.LoaderExceptions.Select(static e => e?.Message ?? "unknown"))));
            return false;
        }

        foreach (var name in new[] { "Instance", "Descriptor" })
        {
            if (TryReadStaticDescriptor(types, name, out descriptor))
            {
                return true;
            }
        }

        foreach (var type in types.OrderBy(static t => t.FullName, StringComparer.Ordinal))
        {
            if (TryReadStaticDescriptor(new[] { type }, name: null, out descriptor))
            {
                return true;
            }
        }

        refusal = new ComponentRefusal(
            ComponentRefusalCodes.DescriptorMissing,
            "'" + assembly.GetName().Name + "' exposes no public static ComponentDescriptor (name it 'Instance').");
        return false;
    }

    private static bool TryReadStaticDescriptor(IEnumerable<Type> types, string? name, out ComponentDescriptor? descriptor)
    {
        descriptor = null;
        foreach (var type in types)
        {
            const BindingFlags Flags = BindingFlags.Public | BindingFlags.Static;
            var properties = type.GetProperties(Flags)
                .Where(p => p.PropertyType == typeof(ComponentDescriptor)
                    && p.CanRead && p.GetIndexParameters().Length == 0
                    && (name is null || string.Equals(p.Name, name, StringComparison.Ordinal)))
                .OrderBy(static p => p.Name, StringComparer.Ordinal);
            foreach (var property in properties)
            {
                descriptor = property.GetValue(null) as ComponentDescriptor;
                if (descriptor is not null)
                {
                    return true;
                }
            }

            var fields = type.GetFields(Flags)
                .Where(f => f.FieldType == typeof(ComponentDescriptor)
                    && (name is null || string.Equals(f.Name, name, StringComparison.Ordinal)))
                .OrderBy(static f => f.Name, StringComparer.Ordinal);
            foreach (var field in fields)
            {
                descriptor = field.GetValue(null) as ComponentDescriptor;
                if (descriptor is not null)
                {
                    return true;
                }
            }
        }

        return false;
    }
}
