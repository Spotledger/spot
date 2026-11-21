using Avalonia.Headless.XUnit;
using Spotledger.Component.Contract;
using Xunit;

namespace Spotledger.Component.TestKit;

/// <summary>
/// The reusable contract suite. A component package becomes tested by deriving from this class
/// and naming its descriptor; every fact below then runs against it headlessly. Each fact asserts
/// the kit found no violation, so a package that breaks a rule fails with the specific code and
/// sentence the check produced.
/// </summary>
/// <typeparam name="T">The component type the package ships.</typeparam>
public abstract class ComponentContractTests<T>
    where T : IComponent
{
    /// <summary>The descriptor the package publishes (the same one the store would sign).</summary>
    protected abstract ComponentDescriptor Descriptor { get; }

    /// <summary>Every fact reports the violations it found, one line each, so failures are readable.</summary>
    private protected static void AssertContract(IReadOnlyList<ComponentContractViolation> violations)
    {
        ArgumentNullException.ThrowIfNull(violations);
        Assert.True(
            violations.Count == 0,
            "The component breaks the contract in " + violations.Count + " place(s):" + Environment.NewLine
            + string.Join(Environment.NewLine, violations.Select(v => "  - " + v)));
    }

    /// <summary>The descriptor is sound and the instance declarations are complete.</summary>
    [AvaloniaFact]
    public void DescriptorIsSound()
    {
        AssertContract(ComponentContractKit.CheckDescriptor(Descriptor));
    }

    /// <summary>The props schema parses, binds only supported field types and validates prop sets.</summary>
    [AvaloniaFact]
    public void PropsSchemaIsSound()
    {
        AssertContract(ComponentContractKit.CheckPropsSchema(Descriptor));
    }

    /// <summary>Every supported field type round-trips a canonical, a null and an empty value.</summary>
    [AvaloniaFact]
    public void ValueRoundTripsForEverySupportedFieldType()
    {
        var violations = new List<ComponentContractViolation>();
        foreach (var type in Descriptor.SupportedFieldTypes)
        {
            using var component = CreateFor(type);
            Assert.IsAssignableFrom<T>(component);
            violations.AddRange(ComponentContractKit.CheckValueRoundTrip(component));
        }

        AssertContract(violations);
    }

    /// <summary>A read-only binding is shown but never changed by the user.</summary>
    [AvaloniaFact]
    public void ReadOnlyBindingIsNotEditable()
    {
        var violations = Descriptor.SupportedFieldTypes
            .SelectMany(type => ComponentContractKit.CheckReadOnly(Descriptor, type))
            .ToList();
        AssertContract(violations);
    }

    /// <summary>A disabled binding is disabled and inert.</summary>
    [AvaloniaFact]
    public void DisabledBindingIsInert()
    {
        var violations = Descriptor.SupportedFieldTypes
            .SelectMany(type => ComponentContractKit.CheckDisabled(Descriptor, type))
            .ToList();
        AssertContract(violations);
    }

    /// <summary>Validation refuses bad values and surfaces the message to assistive technology.</summary>
    [AvaloniaFact]
    public void ValidationMessagesAreSurfaced()
    {
        var violations = Descriptor.SupportedFieldTypes
            .SelectMany(type => ComponentContractKit.CheckValidation(Descriptor, type))
            .ToList();
        AssertContract(violations);
    }

    /// <summary>The styles use design tokens only, and the ControlTheme actually applies.</summary>
    [AvaloniaFact]
    public void StylesUseOnlyDesignTokens()
    {
        AssertContract(ComponentContractKit.CheckThemeUsage(Descriptor));
    }

    /// <summary>Layout mirrors under right-to-left and the Urdu label renders.</summary>
    [AvaloniaFact]
    public void MirrorsUnderRightToLeftAndRendersUrdu()
    {
        var violations = Descriptor.SupportedFieldTypes
            .SelectMany(type => ComponentContractKit.CheckRightToLeft(Descriptor, type))
            .ToList();
        AssertContract(violations);
    }

    /// <summary>The whole component is operable from the keyboard alone.</summary>
    [AvaloniaFact]
    public void OperatesKeyboardOnly()
    {
        var violations = Descriptor.SupportedFieldTypes
            .SelectMany(type => ComponentContractKit.CheckKeyboard(Descriptor, type))
            .ToList();
        AssertContract(violations);
    }

    /// <summary>The automation peer announces a name and the declared role.</summary>
    [AvaloniaFact]
    public void AccessibilityTreeHasNameAndRole()
    {
        var violations = Descriptor.SupportedFieldTypes
            .SelectMany(type => ComponentContractKit.CheckAccessibility(Descriptor, type))
            .ToList();
        AssertContract(violations);
    }

    /// <summary>1,000 updates stay inside the declared performance budget.</summary>
    [AvaloniaFact]
    public void MeetsThousandUpdateBudget()
    {
        var violations = new List<ComponentContractViolation>();
        foreach (var type in Descriptor.SupportedFieldTypes)
        {
            using var component = CreateFor(type);
            violations.AddRange(ComponentContractKit.CheckPerformance(component));
        }

        AssertContract(violations);
    }

    /// <summary>
    /// The package references only the contract, Avalonia and the base library. A static review
    /// aid, NOT a sandbox: a pass does not mean the component is safe (ADR-081).
    /// </summary>
    [AvaloniaFact]
    public void ReferencesOnlyAllowedAssemblies()
    {
        AssertContract(ComponentContractKit.CheckAssemblyReferences(Descriptor));
    }

    /// <summary>Disposing the component releases it and its view.</summary>
    [AvaloniaFact]
    public void DisposeReleasesTheComponent()
    {
        AssertContract(ComponentContractKit.CheckDisposal(Descriptor));
    }

    private IComponent CreateFor(ComponentFieldType type) =>
        Descriptor.Create(TestContexts.Create(Descriptor, type));
}
