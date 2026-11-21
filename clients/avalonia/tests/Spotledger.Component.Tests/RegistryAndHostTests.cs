using Acme.RatingComponent;
using Avalonia.Headless.XUnit;
using Spotledger.Component.Contract;
using Spotledger.Component.TestKit;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// The host side: a registered component is created for a matching field, and refused with a
/// stable reason for an unknown id or a field type it never claimed.
/// </summary>
public sealed class RegistryAndHostTests
{
    [AvaloniaFact]
    public void HostCreatesTheComponentForAMatchingField()
    {
        var registry = new ComponentRegistry();
        Assert.True(registry.Register(AcmeRatingDescriptor.Instance).Accepted);
        var host = new ComponentHost(registry);
        var context = TestContexts.Create(AcmeRatingDescriptor.Instance, ComponentFieldType.IntegerNumber);

        Assert.True(host.TryCreate(AcmeRatingDescriptor.ComponentId, context, out var component, out var refusal));
        Assert.Null(refusal);
        Assert.IsType<AcmeRatingComponent>(component);
        Assert.Equal(ComponentValue.FromInteger(3), component!.Value);
        component.Dispose();
    }

    [AvaloniaFact]
    public void HostRefusesAnUnknownComponent()
    {
        var host = new ComponentHost(new ComponentRegistry());
        var context = TestContexts.Create(AcmeRatingDescriptor.Instance, ComponentFieldType.IntegerNumber);

        Assert.False(host.TryCreate("acme.nope", context, out var component, out var refusal));
        Assert.Null(component);
        Assert.Contains("component.unknown", refusal, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void HostRefusesAFieldTypeTheComponentDoesNotClaim()
    {
        var registry = new ComponentRegistry();
        Assert.True(registry.Register(AcmeRatingDescriptor.Instance).Accepted);
        var host = new ComponentHost(registry);
        var context = TestContexts.Create(AcmeRatingDescriptor.Instance, ComponentFieldType.Text);

        Assert.False(host.TryCreate(AcmeRatingDescriptor.ComponentId, context, out _, out var refusal));
        Assert.Contains("component.field_type.unsupported", refusal, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void ContextUsesTheTokensItIsGiven()
    {
        var context = TestContexts.Create(AcmeRatingDescriptor.Instance, ComponentFieldType.IntegerNumber);

        Assert.True(context.Tokens.TryGet("Color.Brand.500", out var brand));
        Assert.NotNull(brand);
        Assert.True(context.Tokens.GetLength("Space.2", 0) > 0);
        Assert.NotNull(context.Tokens.GetColor("Color.Text.Primary"));
        Assert.NotEmpty(context.Tokens.Keys);
    }

    [Fact]
    public void ContextRefusesAValueOfAnotherType()
    {
        var field = new FieldMetadata("rating", "Rating", ComponentFieldType.IntegerNumber);

        Assert.Throws<ArgumentException>(() => new ComponentContext(ComponentValue.FromText("three"), field));
    }

    [Fact]
    public void ReadOnlyFieldMetadataMakesTheContextReadOnly()
    {
        var field = new FieldMetadata("rating", "Rating", ComponentFieldType.IntegerNumber, ReadOnly: true);
        var context = new ComponentContext(ComponentValue.FromInteger(2), field);

        Assert.True(context.IsReadOnly);
    }
}
