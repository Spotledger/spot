using Avalonia.Headless.XUnit;
using Spotledger.Component.Contract;
using Spotledger.Component.TestKit;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// Proves the kit bites: each deliberately broken package is run through the check that should
/// catch it, and the test fails unless that exact violation code comes back.
/// </summary>
public sealed class BrokenVariantTests
{
    [AvaloniaFact]
    public void HardCodedColourIsRejectedByTheTokenCheck()
    {
        var violations = ComponentContractKit.CheckThemeUsage(BrokenRatings.HardCodedColour);

        Assert.Contains(
            violations,
            violation => violation.Code == ViolationCodes.TokenLiteral
                && violation.Message.Contains("#FFD700", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public void HardCodedColourPackageIsOtherwiseSound()
    {
        // Only the styling check may complain: the package's descriptor, schema and declarations
        // are complete, so the kit is not merely refusing everything about it.
        Assert.Empty(ComponentContractKit.CheckDescriptor(BrokenRatings.HardCodedColour));
        Assert.Empty(ComponentContractKit.CheckPropsSchema(BrokenRatings.HardCodedColour));
    }

    [AvaloniaFact]
    public void MissingAccessibilityNameIsRejected()
    {
        var violations = ComponentContractKit.CheckAccessibility(BrokenRatings.Unnamed, ComponentFieldType.IntegerNumber);

        Assert.Contains(
            violations,
            violation => violation.Code == ViolationCodes.Accessibility
                && violation.Message.Contains("Name is empty", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public void MissingAccessibilityNameIsRejectedByTheDescriptorCheckToo()
    {
        var violations = ComponentContractKit.CheckDescriptor(BrokenRatings.Unnamed);

        Assert.Contains(
            violations,
            violation => violation.Code == ViolationCodes.Accessibility
                && violation.Message.Contains("accessibility.name.missing", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public void SoundVariantOfTheSameShapePassesTheSameCheck()
    {
        // The negative control: the unnamed package differs from a sound one only in its name, so
        // a passing run here proves the failure above is about the name, not the harness.
        var violations = ComponentContractKit.CheckAccessibility(BrokenRatings.HardCodedColour, ComponentFieldType.IntegerNumber);
        Assert.DoesNotContain(violations, violation => violation.Message.Contains("Name is empty", StringComparison.Ordinal));
    }
}
