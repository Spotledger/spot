using Acme.RatingComponent;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Spotledger.Component.Contract;
using Spotledger.Component.TestKit;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// <c>TryCreate</c> promises a bool and a refusal. Author code in the factory can throw, return
/// nothing or hand back a half-built component; none of that may escape or leave an instance behind.
/// </summary>
public sealed class HostCreateFailureTests
{
    private static ComponentHost HostFor(Func<ComponentContext, IComponent> factory, out ComponentDescriptor descriptor)
    {
        descriptor = new ComponentDescriptor(
            "acme.fragile",
            "Fragile",
            new ComponentVersion(1, 0, 0),
            "acme",
            ContractVersionRange.ForMajor(ComponentContract.Version),
            new[] { ComponentFieldType.IntegerNumber },
            AcmeRatingDescriptor.PropsSchemaJson,
            AcmeRatingDescriptor.ThemeSource,
            factory);
        var registry = new ComponentRegistry();
        Assert.True(registry.Register(descriptor).Accepted);
        return new ComponentHost(registry);
    }

    [AvaloniaFact]
    public void AThrowingFactoryIsARefusalWithoutTheMessage()
    {
        var host = HostFor(static _ => throw new InvalidOperationException("secret-token-123 at /home/x"), out var descriptor);
        var context = TestContexts.Create(descriptor, ComponentFieldType.IntegerNumber);

        Assert.False(host.TryCreate("acme.fragile", context, out var component, out var refusal));

        Assert.Null(component);
        Assert.Equal("component.create_failed: acme.fragile could not be created.", refusal);
        Assert.DoesNotContain("secret", refusal, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void AFactoryReturningNullIsARefusal()
    {
        var host = HostFor(static _ => null!, out var descriptor);
        var context = TestContexts.Create(descriptor, ComponentFieldType.IntegerNumber);

        Assert.False(host.TryCreate("acme.fragile", context, out var component, out var refusal));

        Assert.Null(component);
        Assert.Equal("component.create_failed: acme.fragile could not be created.", refusal);
    }

    [AvaloniaFact]
    public void AHalfBuiltComponentThatThrowsIsDisposed()
    {
        var half = new HalfBuilt(throwFromView: true);
        var host = HostFor(_ => half, out var descriptor);
        var context = TestContexts.Create(descriptor, ComponentFieldType.IntegerNumber);

        Assert.False(host.TryCreate("acme.fragile", context, out var component, out var refusal));

        Assert.Null(component);
        Assert.StartsWith("component.create_failed:", refusal, StringComparison.Ordinal);
        Assert.Equal(1, half.Disposals);
    }

    [AvaloniaFact]
    public void AComponentWithoutAViewIsDisposedAndRefused()
    {
        var half = new HalfBuilt(throwFromView: false);
        var host = HostFor(_ => half, out var descriptor);
        var context = TestContexts.Create(descriptor, ComponentFieldType.IntegerNumber);

        Assert.False(host.TryCreate("acme.fragile", context, out var component, out _));

        Assert.Null(component);
        Assert.Equal(1, half.Disposals);
    }

    [AvaloniaFact]
    public void AGoodFactoryStillCreates()
    {
        var host = HostFor(static context => new AcmeRatingComponent(context), out var descriptor);
        var context = TestContexts.Create(descriptor, ComponentFieldType.IntegerNumber);

        Assert.True(host.TryCreate("acme.fragile", context, out var component, out var refusal));
        Assert.Null(refusal);
        component!.Dispose();
    }

    /// <summary>A component whose view is missing or throws; counts how often it was disposed.</summary>
    private sealed class HalfBuilt : IComponent
    {
        private readonly bool _throwFromView;

        public HalfBuilt(bool throwFromView) => _throwFromView = throwFromView;

        public int Disposals { get; private set; }

        public ComponentDescriptor Descriptor => throw new NotSupportedException();

        public Control View => _throwFromView ? throw new InvalidOperationException("no view") : null!;

        public ComponentValue Value => ComponentValue.Null(ComponentFieldType.IntegerNumber);

        public ComponentAccessibility Accessibility => throw new NotSupportedException();

        public KeyboardContract Keyboard => throw new NotSupportedException();

        public PerformanceContract Performance => throw new NotSupportedException();

#pragma warning disable CS0067 // never raised: this stub is never run
        public event EventHandler<ComponentValueChangedEventArgs>? ValueChanged;

        public event EventHandler<ComponentCommittedEventArgs>? Committed;

        public event EventHandler<ComponentCancelledEventArgs>? Cancelled;

        public event EventHandler<ComponentValidatedEventArgs>? Validated;
#pragma warning restore CS0067

        public void SetValue(ComponentValue value)
        {
        }

        public IReadOnlyList<ValidationMessage> Validate(ComponentValue proposed) => Array.Empty<ValidationMessage>();

        public void Dispose() => Disposals++;
    }
}
