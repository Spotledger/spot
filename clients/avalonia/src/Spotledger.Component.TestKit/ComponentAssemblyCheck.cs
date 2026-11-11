using System.Globalization;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Reflection.PortableExecutable;
using Spotledger.Component.Contract;

namespace Spotledger.Component.TestKit;

/// <summary>
/// The one table behind the assembly-reference check: which assemblies a tier B package may
/// reference, which are banned and why. It follows the SPK-08 report
/// (<c>docs/spikes/SPK-08/REPORT.md</c>): a desktop package is fully trusted in-process code that
/// references the contract, Avalonia and the .NET base library, and nothing of the host.
/// </summary>
/// <remarks>
/// This is a REVIEW AID, not a sandbox (ADR-081). It reads metadata statically and proves
/// nothing about what the component does at run time.
/// </remarks>
public static class ComponentAssemblyPolicy
{
    /// <summary>The contract assembly: the only Spotledger assembly a package may reference.</summary>
    public const string ContractAssembly = "Spotledger.Component.Contract";

    /// <summary>Assembly names (and everything under them as a dotted prefix) that are always refused, with the reason.</summary>
    public static IReadOnlyList<KeyValuePair<string, string>> Banned { get; } = new[]
    {
        new KeyValuePair<string, string>("Spotledger.Client", "a host-internal assembly; a package talks to the host only through the contract"),
        new KeyValuePair<string, string>("System.Diagnostics.Process", "starts child processes"),
        new KeyValuePair<string, string>("System.Reflection.Emit", "generates code at run time"),
        new KeyValuePair<string, string>("System.Runtime.Loader", "loads further assemblies behind the review"),
        new KeyValuePair<string, string>("System.Net", "network access (sockets, HTTP, web sockets, requests)"),
        new KeyValuePair<string, string>("System.IO.FileSystem", "file system access"),
        new KeyValuePair<string, string>("System.IO.Pipes", "inter-process pipes"),
        new KeyValuePair<string, string>("System.IO.MemoryMappedFiles", "memory-mapped files"),
    };

    /// <summary>The sentence every message and the summary line end with.</summary>
    public const string ReviewAidSentence = "This check is a review aid, not a sandbox.";

    private static readonly KeyValuePair<string, string>[] TypeRoutes =
    {
        new("System.IO.File", "file access"),
        new("System.IO.FileStream", "file access"),
        new("System.IO.FileInfo", "file access"),
        new("System.IO.Directory", "file access"),
        new("System.IO.DirectoryInfo", "file access"),
        new("System.Diagnostics.Process", "starts child processes"),
        new("System.Diagnostics.ProcessStartInfo", "starts child processes"),
        new("System.Runtime.InteropServices.NativeLibrary", "loads native code"),
    };

    /// <summary>Base-library types refused even though their assembly is on the allowed list (they live in the core library).</summary>
    public static IReadOnlyList<string> BannedTypes { get; } = TypeRoutes.Select(entry => entry.Key).ToArray();

    /// <summary>Namespaces (and everything under them) whose types are refused: code generation, loaders, the dynamic binder.</summary>
    public static IReadOnlyList<string> BannedTypeNamespaces { get; } = new[]
    {
        "System.Reflection.Emit",
        "System.Runtime.Loader",
        "Microsoft.CSharp.RuntimeBinder",
    };

    /// <summary>
    /// Member references refused by type and member name, with the route. The generic
    /// <c>Activator.CreateInstance&lt;T&gt;()</c> that <c>new T()</c> compiles to is not matched; the
    /// overloads that take a <c>Type</c> (possibly one obtained by name) are.
    /// </summary>
    public static IReadOnlyList<(string Type, string Member, string Route)> BannedMembers { get; } = new (string, string, string)[]
    {
        ("System.Reflection.Assembly", "Load", "loads an assembly at run time"),
        ("System.Reflection.Assembly", "LoadFrom", "loads an assembly at run time"),
        ("System.Reflection.Assembly", "LoadFile", "loads an assembly at run time"),
        ("System.Reflection.Assembly", "UnsafeLoadFrom", "loads an assembly at run time"),
        ("System.Reflection.Assembly", "LoadWithPartialName", "loads an assembly at run time"),
        ("System.Activator", "CreateInstance", "creates an instance of a type chosen at run time"),
        ("System.Activator", "CreateInstanceFrom", "creates an instance of a type chosen at run time"),
        ("System.Type", "GetType", "resolves a type by name at run time"),
        ("System.Type", "GetTypeFromProgID", "resolves a type by name at run time"),
        ("System.Type", "GetTypeFromCLSID", "resolves a type by name at run time"),
        ("System.Runtime.InteropServices.Marshal", "GetDelegateForFunctionPointer", "calls native code through a function pointer"),
        ("System.Runtime.InteropServices.Marshal", "GetFunctionPointerForDelegate", "hands a native function pointer to native code"),
    };

    /// <summary>The route for a banned type full name, or null.</summary>
    public static string? TypeRoute(string fullName)
    {
        foreach (var entry in TypeRoutes)
        {
            if (string.Equals(entry.Key, fullName, StringComparison.Ordinal))
            {
                return entry.Value;
            }
        }

        foreach (var prefix in BannedTypeNamespaces)
        {
            if (fullName.StartsWith(prefix + ".", StringComparison.Ordinal))
            {
                return prefix switch
                {
                    "System.Reflection.Emit" => "generates code at run time",
                    "System.Runtime.Loader" => "loads code at run time",
                    _ => "uses dynamic (late-bound) call sites",
                };
            }
        }

        return null;
    }

    /// <summary>The verdict for one referenced assembly name.</summary>
    public static string? Refusal(string assemblyName)
    {
        ArgumentNullException.ThrowIfNull(assemblyName);
        foreach (var (prefix, reason) in Banned)
        {
            if (IsNameOrChild(assemblyName, prefix))
            {
                return "references '" + assemblyName + "', which is banned (" + reason + ").";
            }
        }

        return IsAllowed(assemblyName)
            ? null
            : "references '" + assemblyName + "', which is not on the allowed list "
                + "(the contract, Avalonia and the .NET base library).";
    }

    private static bool IsAllowed(string name) =>
        string.Equals(name, ContractAssembly, StringComparison.OrdinalIgnoreCase)
        || IsNameOrChild(name, "Avalonia")
        || string.Equals(name, "mscorlib", StringComparison.OrdinalIgnoreCase)
        || string.Equals(name, "netstandard", StringComparison.OrdinalIgnoreCase)
        || IsNameOrChild(name, "System");

    // Names are compared ignoring case because the runtime binds simple assembly names ignoring case:
    // a hand-made reference to 'System.net.http' reaches System.Net.Http, so an ordinal ban would miss
    // it while the allow-list's 'System.' prefix waved it through. The table matches NAMES only (no
    // version, no public key token), so an assembly a package ships under a System.* or Avalonia.*
    // name passes: another reason this is a review aid and not a sandbox.
    private static bool IsNameOrChild(string name, string prefix) =>
        string.Equals(name, prefix, StringComparison.OrdinalIgnoreCase)
        || name.StartsWith(prefix + ".", StringComparison.OrdinalIgnoreCase);
}

public static partial class ComponentContractKit
{
    private const string StaticCheckNote = "static reference check; does not prove the component is safe";
    private const string ReviewAction = " tier B review: this component reaches code or files outside the contract; the reviewer must read it.";

    /// <summary>
    /// Reads the metadata of the assembly that holds the descriptor's factory (nothing is
    /// executed) and reports every reference outside the allowed table in
    /// <see cref="ComponentAssemblyPolicy"/>.
    /// </summary>
    /// <remarks>
    /// This is a REVIEW AID, NOT A SANDBOX, and not proof that a component is safe. Tier B
    /// components are trusted in-process code (ADR-081). The check is bypassable by a determined
    /// author: reflection and late binding (<c>Type.GetType</c>, <c>Assembly.Load</c>,
    /// <c>dynamic</c>), types reached through an allowed assembly such as <c>System.Runtime</c>
    /// forwarding to the core library, native code in a file the package ships next to its DLL, and
    /// anything a referenced assembly does on the component's behalf (an indirect dependency) are
    /// not seen. Only the assembly that holds the descriptor's factory (<see cref="ComponentDescriptor.FactoryAssembly"/>)
    /// is read: any other assembly the package ships is not checked, and a reference is matched by
    /// simple name only, so a package's own assembly named like a System.* or Avalonia.* one passes. It catches honest mistakes and unreviewed shortcuts, so a reviewer knows where to
    /// look.
    /// </remarks>
    public static IReadOnlyList<ComponentContractViolation> CheckAssemblyReferences(ComponentDescriptor descriptor)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var location = descriptor.FactoryAssembly is { IsDynamic: false } assembly ? assembly.Location : string.Empty;
        if (string.IsNullOrEmpty(location) || !File.Exists(location))
        {
            return new[] { Reference("the component assembly has no file the kit can read, so its references were not checked") };
        }

        try
        {
            using var stream = File.OpenRead(location);
            using var image = new PEReader(stream);
            if (!image.HasMetadata)
            {
                return new[] { Reference("the component assembly has no .NET metadata") };
            }

            return CheckAssemblyReferences(image.GetMetadataReader());
        }
        catch (Exception error) when (error is IOException or BadImageFormatException or UnauthorizedAccessException)
        {
            return new[] { Reference("the component assembly could not be read (" + error.GetType().Name + ")") };
        }
    }

    /// <summary>
    /// The same check over metadata already in hand (a signed package's DLL read by the store, or
    /// a metadata-only fixture in a test). Also reports a P/Invoke (<c>DllImport</c>), an
    /// unmanaged function-pointer call, and a use of a banned base-library type. A REVIEW AID, NOT
    /// A SANDBOX: see the remarks on <see cref="CheckAssemblyReferences(ComponentDescriptor)"/>.
    /// </summary>
    public static IReadOnlyList<ComponentContractViolation> CheckAssemblyReferences(MetadataReader reader)
    {
        ArgumentNullException.ThrowIfNull(reader);
        var violations = new List<ComponentContractViolation>();
        foreach (var handle in reader.AssemblyReferences)
        {
            var name = reader.GetString(reader.GetAssemblyReference(handle).Name);
            if (ComponentAssemblyPolicy.Refusal(name) is { } refusal)
            {
                violations.Add(Reference(refusal));
            }
        }

        foreach (var handle in reader.TypeReferences)
        {
            var type = reader.GetTypeReference(handle);
            var fullName = Qualified(reader.GetString(type.Namespace), reader.GetString(type.Name));
            if (ComponentAssemblyPolicy.TypeRoute(fullName) is { } route)
            {
                violations.Add(Reference("uses the banned type '" + fullName + "' (" + route + ")."));
            }
        }

        foreach (var handle in reader.MemberReferences)
        {
            var member = reader.GetMemberReference(handle);
            if (member.Parent.Kind != HandleKind.TypeReference)
            {
                continue;
            }

            var owner = reader.GetTypeReference((TypeReferenceHandle)member.Parent);
            var ownerName = Qualified(reader.GetString(owner.Namespace), reader.GetString(owner.Name));
            var memberName = reader.GetString(member.Name);
            foreach (var (type, name, route) in ComponentAssemblyPolicy.BannedMembers)
            {
                if (string.Equals(type, ownerName, StringComparison.Ordinal)
                    && string.Equals(name, memberName, StringComparison.Ordinal)
                    && !IsGenericCreateInstance(reader, member, type))
                {
                    violations.Add(Reference("calls '" + ownerName + "." + memberName + "' (" + route + ")."));
                }
            }
        }

        foreach (var handle in reader.MethodDefinitions)
        {
            var method = reader.GetMethodDefinition(handle);
            var import = method.GetImport();
            if (import.Module.IsNil)
            {
                continue;
            }

            var owner = reader.GetTypeDefinition(method.GetDeclaringType());
            violations.Add(Reference(
                "declares a P/Invoke '" + Qualified(reader.GetString(owner.Namespace), reader.GetString(owner.Name))
                + "." + reader.GetString(method.Name) + "' into native library '"
                + reader.GetString(reader.GetModuleReference(import.Module).Name) + "'."));
        }

        var pointerCalls = 0;
        for (var row = 1; row <= reader.GetTableRowCount(TableIndex.StandAloneSig); row++)
        {
            var handle = MetadataTokens.StandaloneSignatureHandle(row);
            var signature = reader.GetStandaloneSignature(handle);
            var blob = reader.GetBlobReader(signature.Signature);
            if (blob.Length == 0)
            {
                continue;
            }

            var header = blob.ReadSignatureHeader();
            if (header.Kind == SignatureKind.Method && header.CallingConvention is
                SignatureCallingConvention.CDecl or SignatureCallingConvention.StdCall
                or SignatureCallingConvention.ThisCall or SignatureCallingConvention.FastCall
                or SignatureCallingConvention.Unmanaged)
            {
                pointerCalls++;
            }
        }

        if (pointerCalls > 0)
        {
            violations.Add(Reference(
                "makes " + pointerCalls.ToString(CultureInfo.InvariantCulture)
                + " unmanaged function-pointer call(s) (a native import without a DllImport)."));
        }

        return violations;
    }

    /// <summary>
    /// One summary line for the assembly-reference violations. It always ends with the fixed
    /// sentence that the check is a review aid and not a sandbox.
    /// </summary>
    public static string SummariseAssemblyReferences(IReadOnlyList<ComponentContractViolation> violations)
    {
        ArgumentNullException.ThrowIfNull(violations);
        var count = violations.Count(v => v.Code == ViolationCodes.AssemblyReference);
        return count.ToString(CultureInfo.InvariantCulture) + " assembly reference finding(s). "
            + ComponentAssemblyPolicy.ReviewAidSentence;
    }

    // new T() compiles to the generic Activator.CreateInstance<T>(): a type fixed at compile time.
    private static bool IsGenericCreateInstance(MetadataReader reader, MemberReference member, string type)
    {
        if (!string.Equals(type, "System.Activator", StringComparison.Ordinal))
        {
            return false;
        }

        var blob = reader.GetBlobReader(member.Signature);
        return blob.Length > 0 && blob.ReadSignatureHeader().IsGeneric;
    }

    private static ComponentContractViolation Reference(string detail) =>
        new(ViolationCodes.AssemblyReference, detail + " [" + StaticCheckNote + "]" + ReviewAction + " " + ComponentAssemblyPolicy.ReviewAidSentence);

    private static string Qualified(string ns, string name) => ns.Length == 0 ? name : ns + "." + name;
}
