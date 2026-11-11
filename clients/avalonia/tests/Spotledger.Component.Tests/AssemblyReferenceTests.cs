using System.Reflection;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using Acme.RatingComponent;
using Spotledger.Component.TestKit;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// The assembly-reference check is a static REVIEW AID, not a sandbox. These tests prove what it
/// catches, using metadata-only fixtures (nothing is loaded or executed), and that the sample
/// package passes. They prove nothing about components that bypass the check by reflection.
/// </summary>
public sealed class AssemblyReferenceTests
{
    private static readonly string[] AllowedSet =
        { "System.Runtime", "Avalonia.Controls", "Avalonia.Base", "Spotledger.Component.Contract" };

    [Fact]
    public void SamplePackagePasses()
    {
        Assert.Empty(ComponentContractKit.CheckAssemblyReferences(AcmeRatingDescriptor.Instance));
    }

    [Fact]
    public void AnAssemblyWithAllowedReferencesPasses()
    {
        var reader = MetadataFixture.Build(AllowedSet);

        Assert.Empty(ComponentContractKit.CheckAssemblyReferences(reader));
    }

    [Theory]
    [InlineData("Spotledger.Client.Shell")]
    [InlineData("Spotledger.Client")]
    [InlineData("System.Diagnostics.Process")]
    [InlineData("System.Reflection.Emit")]
    [InlineData("System.Reflection.Emit.Lightweight")]
    [InlineData("System.Runtime.Loader")]
    [InlineData("System.Net.Sockets")]
    [InlineData("System.Net.Http")]
    [InlineData("System.Net.WebSockets")]
    [InlineData("System.Net.Requests")]
    [InlineData("System.IO.FileSystem")]
    [InlineData("System.IO.FileSystem.Watcher")]
    [InlineData("System.IO.Pipes")]
    [InlineData("System.IO.MemoryMappedFiles")]
    [InlineData("System.net.http")]
    [InlineData("SYSTEM.NET.SOCKETS")]
    [InlineData("system.diagnostics.process")]
    [InlineData("spotledger.client.shell")]
    [InlineData("System.Reflection.emit")]
    public void ABannedReferenceFailsAndIsNamed(string banned)
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base.Append(banned).ToArray());

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.Equal(ViolationCodes.AssemblyReference, violation.Code);
        Assert.Contains("'" + banned + "'", violation.Message, StringComparison.Ordinal);
        Assert.Contains("banned", violation.Message, StringComparison.Ordinal);
        Assert.Contains("does not prove the component is safe", violation.Message, StringComparison.Ordinal);
    }

    [Theory]
    [InlineData("Newtonsoft.Json")]
    [InlineData("Spotledger.Platform.Internal")]
    [InlineData("SystemX")]
    public void AnUnlistedReferenceFailsAndIsNamed(string unlisted)
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base.Append(unlisted).ToArray());

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.Contains("'" + unlisted + "'", violation.Message, StringComparison.Ordinal);
        Assert.Contains("not on the allowed list", violation.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void APInvokeIsReported()
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, pInvokeLibrary: "libnative");

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.Contains("P/Invoke", violation.Message, StringComparison.Ordinal);
        Assert.Contains("libnative", violation.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AnUnmanagedFunctionPointerCallIsReported()
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, unmanagedCall: true);

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.Contains("function-pointer", violation.Message, StringComparison.Ordinal);
    }

    [Theory]
    [InlineData("System.IO", "File")]
    [InlineData("System.IO", "Directory")]
    [InlineData("System.IO", "FileStream")]
    [InlineData("System.Diagnostics", "Process")]
    public void ABannedBaseLibraryTypeIsReported(string ns, string name)
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, typeReference: (ns, name));

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.Contains("'" + ns + "." + name + "'", violation.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AnOrdinaryBaseLibraryTypeIsNotReported()
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, typeReference: ("System.IO", "Path"));

        Assert.Empty(ComponentContractKit.CheckAssemblyReferences(reader));
    }

    public const string Sentence = "This check is a review aid, not a sandbox.";

    [Theory]
    [InlineData("System.Runtime.InteropServices", "NativeLibrary", "native code")]
    [InlineData("System.Reflection.Emit", "DynamicMethod", "generates code")]
    [InlineData("System.Reflection.Emit", "AssemblyBuilder", "generates code")]
    [InlineData("System.Runtime.Loader", "AssemblyLoadContext", "loads code")]
    [InlineData("Microsoft.CSharp.RuntimeBinder", "Binder", "dynamic")]
    [InlineData("Microsoft.CSharp.RuntimeBinder", "CSharpArgumentInfo", "dynamic")]
    [InlineData("System.Diagnostics", "ProcessStartInfo", "process")]
    public void ALateBindingOrNativeTypeIsReportedWithItsRouteAndTheReviewAction(string ns, string name, string route)
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, typeReference: (ns, name));

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.Contains("'" + ns + "." + name + "'", violation.Message, StringComparison.Ordinal);
        Assert.Contains(route, violation.Message, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("tier B review", violation.Message, StringComparison.Ordinal);
        Assert.EndsWith(Sentence, violation.Message, StringComparison.Ordinal);
    }

    [Theory]
    [InlineData("System.Reflection", "Assembly", "Load", false)]
    [InlineData("System.Reflection", "Assembly", "LoadFrom", false)]
    [InlineData("System.Reflection", "Assembly", "LoadFile", false)]
    [InlineData("System.Reflection", "Assembly", "UnsafeLoadFrom", false)]
    [InlineData("System.Reflection", "Assembly", "LoadWithPartialName", false)]
    [InlineData("System", "Activator", "CreateInstance", false)]
    [InlineData("System", "Activator", "CreateInstanceFrom", false)]
    [InlineData("System", "Type", "GetType", false)]
    [InlineData("System", "Type", "GetTypeFromProgID", false)]
    [InlineData("System", "Type", "GetTypeFromCLSID", false)]
    [InlineData("System.Runtime.InteropServices", "Marshal", "GetDelegateForFunctionPointer", false)]
    [InlineData("System.Runtime.InteropServices", "Marshal", "GetFunctionPointerForDelegate", false)]
    public void ABannedMemberReferenceIsReportedWithItsRouteAndTheReviewAction(string ns, string type, string member, bool generic)
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, memberReference: (ns, type, member, generic));

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.Equal(ViolationCodes.AssemblyReference, violation.Code);
        Assert.Contains("'" + ns + "." + type + "." + member + "'", violation.Message, StringComparison.Ordinal);
        Assert.Contains("tier B review", violation.Message, StringComparison.Ordinal);
        Assert.EndsWith(Sentence, violation.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AGenericActivatorCreateInstanceFromNewTIsNotReported()
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, memberReference: ("System", "Activator", "CreateInstance", true));

        Assert.Empty(ComponentContractKit.CheckAssemblyReferences(reader));
    }

    [Theory]
    [InlineData("System.Reflection", "Assembly", "GetExecutingAssembly")]
    [InlineData("System", "Object", "GetType")]
    [InlineData("System", "String", "Concat")]
    public void AnOrdinaryMemberReferenceIsNotReported(string ns, string type, string member)
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, memberReference: (ns, type, member, false));

        Assert.Empty(ComponentContractKit.CheckAssemblyReferences(reader));
    }

    [Fact]
    public void APInvokeAndFunctionPointerMessagesCarryTheReviewActionAndTheSentence()
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base, pInvokeLibrary: "libnative", unmanagedCall: true);

        var violations = ComponentContractKit.CheckAssemblyReferences(reader);
        Assert.Equal(2, violations.Count);
        Assert.All(violations, v =>
        {
            Assert.Contains("tier B review", v.Message, StringComparison.Ordinal);
            Assert.EndsWith(Sentence, v.Message, StringComparison.Ordinal);
        });
    }

    [Fact]
    public void EveryViolationMessageEndsWithTheFixedSentence()
    {
        var reader = MetadataFixture.Build(MetadataFixture.Base.Append("System.Net.Http").ToArray());

        var violation = Assert.Single(ComponentContractKit.CheckAssemblyReferences(reader));
        Assert.EndsWith(Sentence, violation.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void TheSummaryLineAlwaysEndsWithTheFixedSentence()
    {
        var clean = ComponentContractKit.SummariseAssemblyReferences(Array.Empty<ComponentContractViolation>());
        var reader = MetadataFixture.Build(MetadataFixture.Base, pInvokeLibrary: "libnative");
        var dirty = ComponentContractKit.SummariseAssemblyReferences(ComponentContractKit.CheckAssemblyReferences(reader));

        Assert.EndsWith(Sentence, clean, StringComparison.Ordinal);
        Assert.EndsWith(Sentence, dirty, StringComparison.Ordinal);
        Assert.Contains("1 ", dirty, StringComparison.Ordinal);
        Assert.DoesNotContain("sandboxed", clean + dirty, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void ADescriptorWhoseAssemblyReferencesTheKitFailsTheCheck()
    {
        // The test assembly references the kit, xunit and more: not on the allowed list.
        var violations = ComponentContractKit.CheckAssemblyReferences(BrokenRatings.Unnamed);

        Assert.NotEmpty(violations);
        Assert.All(violations, v => Assert.Equal(ViolationCodes.AssemblyReference, v.Code));
    }
}

/// <summary>Builds a metadata-only assembly image in memory: never loaded, never executed.</summary>
internal static class MetadataFixture
{
    public static readonly string[] Base = { "System.Runtime" };

    public static MetadataReader Build(
        IReadOnlyList<string> references,
        string? pInvokeLibrary = null,
        bool unmanagedCall = false,
        (string Namespace, string Name)? typeReference = null,
        (string Namespace, string Type, string Member, bool Generic)? memberReference = null)
    {
        var builder = new MetadataBuilder();
        builder.AddModule(0, builder.GetOrAddString("fixture.dll"), builder.GetOrAddGuid(Guid.NewGuid()), default, default);
        builder.AddAssembly(builder.GetOrAddString("Fixture"), new Version(1, 0, 0, 0), default, default, 0, AssemblyHashAlgorithm.None);
        AssemblyReferenceHandle first = default;
        foreach (var name in references)
        {
            var handle = builder.AddAssemblyReference(builder.GetOrAddString(name), new Version(1, 0, 0, 0), default, default, 0, default);
            if (first.IsNil)
            {
                first = handle;
            }
        }

        if (typeReference is { } type)
        {
            builder.AddTypeReference(first, builder.GetOrAddString(type.Namespace), builder.GetOrAddString(type.Name));
        }

        if (memberReference is { } member)
        {
            var owner = builder.AddTypeReference(first, builder.GetOrAddString(member.Namespace), builder.GetOrAddString(member.Type));
            var signature = new BlobBuilder();
            new BlobEncoder(signature)
                .MethodSignature(genericParameterCount: member.Generic ? 1 : 0)
                .Parameters(0, returnType => returnType.Void(), _ => { });
            builder.AddMemberReference(owner, builder.GetOrAddString(member.Member), builder.GetOrAddBlob(signature));
        }

        var voidSignature = new BlobBuilder();
        new BlobEncoder(voidSignature).MethodSignature().Parameters(0, returnType => returnType.Void(), _ => { });
        var method = builder.AddMethodDefinition(
            MethodAttributes.Public | MethodAttributes.Static | (pInvokeLibrary is null ? 0 : MethodAttributes.PinvokeImpl),
            MethodImplAttributes.IL,
            builder.GetOrAddString("Run"),
            builder.GetOrAddBlob(voidSignature),
            -1,
            default);
        builder.AddTypeDefinition(
            TypeAttributes.Public | TypeAttributes.Class,
            builder.GetOrAddString("Fixture"),
            builder.GetOrAddString("Native"),
            default,
            MetadataTokens.FieldDefinitionHandle(1),
            method);
        if (pInvokeLibrary is not null)
        {
            builder.AddMethodImport(
                method,
                MethodImportAttributes.None,
                builder.GetOrAddString("run"),
                builder.AddModuleReference(builder.GetOrAddString(pInvokeLibrary)));
        }

        if (unmanagedCall)
        {
            var pointer = new BlobBuilder();
            new BlobEncoder(pointer)
                .MethodSignature(SignatureCallingConvention.Unmanaged)
                .Parameters(0, returnType => returnType.Void(), _ => { });
            builder.AddStandaloneSignature(builder.GetOrAddBlob(pointer));
        }

        var image = new BlobBuilder();
        new MetadataRootBuilder(builder).Serialize(image, 0, 0);
        return MetadataReaderProvider.FromMetadataImage(image.ToImmutableArray()).GetMetadataReader();
    }
}
