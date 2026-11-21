using System.Text.Json;
using Spotledger.Component.Contract;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Registry integration: how a view names a tier B component, and how that name becomes an installed
/// component with checked props — or a specific refusal.
/// <para>
/// The reference syntax is this task's own seam: <c>contracts/ui-ext</c> is tier A only and
/// <c>spot-views</c>' <c>ViewDef</c> has no component field, so nothing in a contract carries a tier B
/// reference yet. The shape here is the smallest one that can be parsed and validated, and the gap is
/// listed under <c>## Open questions</c> in the task's report.
/// </para>
/// </summary>
public sealed class RegistryIntegrationTests
{
    private static readonly Vectors VectorSet = Vectors.Load();

    [Fact]
    public void AReferenceParsesItsIdAndProps()
    {
        Assert.True(
            ComponentReference.TryParse(
                """{"component_id":"acme.rating","props":{"max":5}}""",
                out var reference,
                out var problems),
            string.Join(" ", problems));

        Assert.NotNull(reference);
        Assert.Equal("acme.rating", reference.ComponentId);
        Assert.Equal(5, reference.Props["max"].GetInt32());
    }

    [Theory]
    [InlineData("")]
    [InlineData("not json")]
    [InlineData("[]")]
    [InlineData("""{"props":{}}""")]
    [InlineData("""{"component_id":7}""")]
    [InlineData("""{"component_id":"acme.rating","props":[]}""")]
    [InlineData("""{"component_id":"acme.rating","name":"x"}""")]
    public void ABadReferenceIsRefusedWithEveryProblem(string json)
    {
        Assert.False(ComponentReference.TryParse(json, out var reference, out var problems));
        Assert.Null(reference);
        Assert.NotEmpty(problems);
    }

    [Fact]
    public void AViewThatNamesAnInstalledComponentResolvesItWithCheckedProps()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);

        var resolution = resolver.Resolve(
            IntegerField(),
            ComponentReference.Create("acme.rating", Props(("max", "4"))));

        Assert.True(resolution.IsResolved, resolution.Refusal?.ToString());
        Assert.Equal("acme.rating", resolution.Resolved!.Descriptor.Id);
        Assert.Equal(4, resolution.Resolved.Props["max"].TryGetInteger(out var max) ? max : -1);
        Assert.NotNull(resolution.Resolved.Installed);
    }

    [Fact]
    public void APropTheComponentDoesNotDeclareIsRefused()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);

        var resolution = resolver.Resolve(
            IntegerField(),
            ComponentReference.Create("acme.rating", Props(("stars", "4"))));

        Assert.False(resolution.IsResolved);
        Assert.Equal(ComponentRefusalCodes.PropsRefused, resolution.Refusal!.Code);
        Assert.Contains("stars", resolution.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void APropOfTheWrongTypeIsRefused()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);

        var resolution = resolver.Resolve(
            IntegerField(),
            ComponentReference.Create("acme.rating", Props(("max", "\"five\""))));

        Assert.False(resolution.IsResolved);
        Assert.Equal(ComponentRefusalCodes.PropsRefused, resolution.Refusal!.Code);
    }

    [Fact]
    public void ARequiredPropThatIsMissingIsRefused()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);

        var resolution = resolver.Resolve(IntegerField(), ComponentReference.Create("acme.rating"));

        Assert.False(resolution.IsResolved);
        Assert.Equal(ComponentRefusalCodes.PropsRefused, resolution.Refusal!.Code);
    }

    [Fact]
    public void AComponentThatDoesNotRenderTheFieldsTypeIsRefused()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);
        var textField = new FieldMetadata("note", "Note", ComponentFieldType.Text);

        var resolution = resolver.Resolve(textField, ComponentReference.Create("acme.rating"));

        Assert.False(resolution.IsResolved);
        Assert.Equal(ComponentRefusalCodes.PropsRefused, resolution.Refusal!.Code);
        Assert.Contains("does not render", resolution.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AnIdThatIsNotInstalledIsRefused()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);

        var resolution = resolver.Resolve(IntegerField(), ComponentReference.Create("acme.missing"));

        Assert.False(resolution.IsResolved);
        Assert.Equal(ComponentRefusalCodes.ComponentUnknown, resolution.Refusal!.Code);
    }

    [Fact]
    public void WithNoReferenceTheInstalledComponentForTheFieldTypeIsChosen()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);

        var resolution = resolver.Resolve(IntegerField());

        // The sample's props schema requires 'max', so the automatic choice still checks the props: a
        // component is only created with props its own schema accepts.
        Assert.False(resolution.IsResolved);
        Assert.Equal(ComponentRefusalCodes.PropsRefused, resolution.Refusal!.Code);
    }

    [Fact]
    public void WithNoComponentForTheFieldTypeNothingIsChosen()
    {
        using var harness = new Harness();
        harness.Install();
        var resolver = new ComponentSlotResolver(harness.Service.Catalogue);
        var textField = new FieldMetadata("note", "Note", ComponentFieldType.Text);

        var resolution = resolver.Resolve(textField);

        Assert.False(resolution.IsResolved);
        Assert.Equal(ComponentRefusalCodes.ComponentUnknown, resolution.Refusal!.Code);
        Assert.Contains("no installed component renders text fields", resolution.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void ABuiltInComponentIsChosenWhenNoPackageCoversTheField()
    {
        var builtIns = new ComponentRegistry();
        var registration = builtIns.Register(TestDescriptors.TextOnly("host.text"));
        Assert.True(registration.Accepted, registration.Message);

        var resolver = new ComponentSlotResolver(new ComponentCatalogue(), builtIns);
        var textField = new FieldMetadata("note", "Note", ComponentFieldType.Text);

        var resolution = resolver.Resolve(textField);

        Assert.True(resolution.IsResolved, resolution.Refusal?.ToString());
        Assert.Equal("host.text", resolution.Resolved!.Descriptor.Id);
        Assert.Null(resolution.Resolved.Installed);
    }

    private static FieldMetadata IntegerField() =>
        new("rating", "Rating", ComponentFieldType.IntegerNumber);

    private static Dictionary<string, JsonElement> Props(params (string Name, string Json)[] values)
    {
        var props = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
        foreach (var (name, json) in values)
        {
            using var document = JsonDocument.Parse(json);
            props[name] = document.RootElement.Clone();
        }

        return props;
    }

    /// <summary>The trust material, the install service and a package, disposed together.</summary>
    private sealed class Harness : IDisposable
    {
        private readonly TempPackage _package = new();

        public Harness()
        {
            Trust = new TestTrust(VectorSet);
            Trust.TrustPublisher();
            Service = new ComponentInstallationService(Trust.Store);
            PackageBuilder.Build(_package.Root, PackageBuilder.SampleSpec(Trust));
        }

        public TestTrust Trust { get; }

        public ComponentInstallationService Service { get; }

        public void Install()
        {
            var result = Service.Install(_package.Root, 1_760_000_000);
            Assert.True(result.IsLoaded, result.Refusal?.ToString());
        }

        public void Dispose()
        {
            Service.UninstallAll();
            _package.Dispose();
        }
    }
}
