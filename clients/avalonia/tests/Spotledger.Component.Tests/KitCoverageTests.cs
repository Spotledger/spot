using Avalonia.Headless.XUnit;
using Spotledger.Component.Contract;
using Spotledger.Component.TestKit;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// Proves the kit's type coverage: the round-trip check is driven through every field type the
/// contract knows, using a component that supports all of them, so no type is silently skipped.
/// </summary>
public sealed class KitCoverageTests
{
    [AvaloniaFact]
    public void RoundTripCheckCoversEveryFieldType()
    {
        foreach (var type in ComponentFieldTypes.All)
        {
            using var component = EchoComponentDescriptor.Instance.Create(
                TestContexts.Create(EchoComponentDescriptor.Instance, type));

            var violations = ComponentContractKit.CheckValueRoundTrip(component);
            Assert.True(
                violations.Count == 0,
                type.ToWireName() + ": " + string.Join("; ", violations.Select(v => v.Message)));
        }
    }

    [AvaloniaFact]
    public void PerformanceCheckCoversEveryFieldType()
    {
        foreach (var type in ComponentFieldTypes.All)
        {
            using var component = EchoComponentDescriptor.Instance.Create(
                TestContexts.Create(EchoComponentDescriptor.Instance, type));

            Assert.Empty(ComponentContractKit.CheckPerformance(component));
        }
    }

    [AvaloniaFact]
    public void EchoDescriptorIsSound()
    {
        Assert.Empty(ComponentContractKit.CheckDescriptor(EchoComponentDescriptor.Instance));
        Assert.Empty(ComponentContractKit.CheckPropsSchema(EchoComponentDescriptor.Instance));
    }

    [Fact]
    public void KitRejectsAnEmptyFieldTypeList()
    {
        var violations = ComponentContractKit.CheckDescriptor(new ComponentDescriptor(
            "spot.empty",
            "Empty",
            new ComponentVersion(1, 0, 0),
            "spot",
            ContractVersionRange.CurrentMajor,
            Array.Empty<ComponentFieldType>(),
            EchoComponentDescriptor.SchemaJson,
            new Uri("avares://Spotledger.Component.Tests/Themes/BrokenRating.axaml"),
            static context => new EchoComponent(EchoComponentDescriptor.Instance, context)));

        Assert.Contains(violations, violation => violation.Message.Contains("descriptor.field_types", StringComparison.Ordinal));
    }

    [Fact]
    public void EveryBannedAssemblyReferenceEntryHasAFixtureAndIsReported()
    {
        // Adding an entry to the policy tables without extending the expected list below fails the
        // build; every listed entry must be reported by the check (a review aid, not a sandbox).
        Assert.Equal(ExpectedBannedTypes, ComponentAssemblyPolicy.BannedTypes.OrderBy(t => t, StringComparer.Ordinal));
        Assert.Equal(ExpectedBannedNamespaces, ComponentAssemblyPolicy.BannedTypeNamespaces.OrderBy(t => t, StringComparer.Ordinal));
        Assert.Equal(ExpectedBannedMembers, ComponentAssemblyPolicy.BannedMembers.Select(m => m.Type + "." + m.Member).OrderBy(t => t, StringComparer.Ordinal));

        foreach (var type in ComponentAssemblyPolicy.BannedTypes)
        {
            var index = type.LastIndexOf('.');
            AssertReported(MetadataFixture.Build(MetadataFixture.Base, typeReference: (type[..index], type[(index + 1)..])), type);
        }

        foreach (var prefix in ComponentAssemblyPolicy.BannedTypeNamespaces)
        {
            AssertReported(MetadataFixture.Build(MetadataFixture.Base, typeReference: (prefix + ".Sub", "AnyType")), prefix);
        }

        foreach (var (type, member, _) in ComponentAssemblyPolicy.BannedMembers)
        {
            var index = type.LastIndexOf('.');
            AssertReported(
                MetadataFixture.Build(MetadataFixture.Base, memberReference: (type[..index], type[(index + 1)..], member, false)),
                type + "." + member);
        }
    }

    private static void AssertReported(System.Reflection.Metadata.MetadataReader reader, string entry)
    {
        var violations = ComponentContractKit.CheckAssemblyReferences(reader);
        Assert.True(violations.Count > 0, entry + " was not reported");
    }

    private static readonly string[] ExpectedBannedTypes =
    {
        "System.Diagnostics.Process",
        "System.Diagnostics.ProcessStartInfo",
        "System.IO.Directory",
        "System.IO.DirectoryInfo",
        "System.IO.File",
        "System.IO.FileInfo",
        "System.IO.FileStream",
        "System.Runtime.InteropServices.NativeLibrary",
    };

    private static readonly string[] ExpectedBannedNamespaces =
    {
        "Microsoft.CSharp.RuntimeBinder",
        "System.Reflection.Emit",
        "System.Runtime.Loader",
    };

    private static readonly string[] ExpectedBannedMembers =
    {
        "System.Activator.CreateInstance",
        "System.Activator.CreateInstanceFrom",
        "System.Reflection.Assembly.Load",
        "System.Reflection.Assembly.LoadFile",
        "System.Reflection.Assembly.LoadFrom",
        "System.Reflection.Assembly.LoadWithPartialName",
        "System.Reflection.Assembly.UnsafeLoadFrom",
        "System.Runtime.InteropServices.Marshal.GetDelegateForFunctionPointer",
        "System.Runtime.InteropServices.Marshal.GetFunctionPointerForDelegate",
        "System.Type.GetType",
        "System.Type.GetTypeFromCLSID",
        "System.Type.GetTypeFromProgID",
    };
}
