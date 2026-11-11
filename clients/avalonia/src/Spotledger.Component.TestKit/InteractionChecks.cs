using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.VisualTree;
using Spotledger.Component.Contract;

namespace Spotledger.Component.TestKit;

/// <summary>
/// The interaction checks: right-to-left mirroring with Urdu text, and keyboard-only operation.
/// They need a shown window, so they live here rather than with the static checks.
/// </summary>
public static partial class ComponentContractKit
{
    /// <summary>
    /// Checks that the same component mirrors under right-to-left: the direction is inherited
    /// (never pinned), the first two named parts swap sides, and the Urdu label actually renders.
    /// </summary>
    public static IReadOnlyList<ComponentContractViolation> CheckRightToLeft(ComponentDescriptor descriptor, ComponentFieldType type)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = new List<ComponentContractViolation>();
        using var leftToRight = ComponentHarness.Show(descriptor, TestContexts.Create(descriptor, type));
        using var rightToLeft = ComponentHarness.Show(
            descriptor,
            TestContexts.Create(descriptor, type, locale: LocaleInfo.Urdu),
            direction: FlowDirection.RightToLeft);

        if (rightToLeft.Component.View.FlowDirection != FlowDirection.RightToLeft)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Rtl, "the view pins FlowDirection=" + rightToLeft.Component.View.FlowDirection
                + " instead of inheriting the host's RightToLeft."));
        }

        // Mirroring is measured on the first panel that arranges at least two named siblings: a
        // horizontal row of parts is what must flip. A component with no such row has nothing the
        // check can measure, so only the direction and the Urdu text are asserted for it.
        var leftRow = MirroredRow(leftToRight);
        var rightRow = MirroredRow(rightToLeft);
        if (leftRow is not null && rightRow is not null)
        {
            var leftFirst = leftToRight.AbsolutePosition(leftRow[0]).X;
            var leftSecond = leftToRight.AbsolutePosition(leftRow[1]).X;
            var rightFirst = rightToLeft.AbsolutePosition(rightRow[0]).X;
            var rightSecond = rightToLeft.AbsolutePosition(rightRow[1]).X;
            if (leftFirst < leftSecond && rightFirst < rightSecond)
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.Rtl, "parts " + leftRow[0].Name + " and " + leftRow[1].Name
                    + " keep left-to-right order under RightToLeft: the layout does not mirror."));
            }
        }

        var label = rightToLeft.Component.View.GetVisualDescendants()
            .OfType<TextBlock>()
            .FirstOrDefault(block => string.Equals(block.Text, UrduSamples.RatingLabel, StringComparison.Ordinal));
        if (label is null)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Urdu, "the Urdu label '" + UrduSamples.RatingLabel + "' is not rendered anywhere in the view."));
        }
        else if (label.Bounds.Width <= 0 || double.IsNaN(label.Bounds.Width))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Urdu, "the Urdu label has no width after layout, so it did not measure."));
        }

        var accessibleName = Avalonia.Automation.AutomationProperties.GetName(rightToLeft.Component.View);
        if (string.IsNullOrWhiteSpace(accessibleName))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Urdu, "the accessible name is empty in the Urdu locale."));
        }

        return violations;
    }

    /// <summary>
    /// Checks keyboard-only operation: the view takes focus, every activation key changes the
    /// value, Enter commits, Escape cancels when handled, and Tab leaves the component.
    /// </summary>
    public static IReadOnlyList<ComponentContractViolation> CheckKeyboard(ComponentDescriptor descriptor, ComponentFieldType type)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = new List<ComponentContractViolation>();
        using var harness = ComponentHarness.Show(descriptor, TestContexts.Create(descriptor, type));
        var component = harness.Component;
        var keyboard = component.Keyboard;

        var trailing = harness.AddTrailingButton();
        harness.FocusView();
        if (!IsWithinView(component, harness.FocusedElement))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Keyboard, "the view could not take keyboard focus."));
        }

        var changes = 0;
        component.ValueChanged += (_, _) => changes++;
        var before = component.Value;
        foreach (var key in keyboard.ActivationKeys)
        {
            changes = 0;
            harness.SendKey(key);
            if (changes == 0 && component.Value == before)
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.Keyboard, "activation key " + key + " did not change the value."));
            }

            before = component.Value;
        }

        if (keyboard.HandledKeys.Contains(ComponentKey.Enter))
        {
            var committed = 0;
            component.Committed += (_, _) => committed++;
            harness.SendKey(ComponentKey.Enter);
            if (committed == 0)
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.Keyboard, "Enter is declared as handled but raised no Committed."));
            }
        }

        if (keyboard.HandledKeys.Contains(ComponentKey.Escape))
        {
            var cancelled = 0;
            component.Cancelled += (_, _) => cancelled++;
            harness.SendKey(ComponentKey.Escape);
            if (cancelled == 0)
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.Keyboard, "Escape is declared as handled but raised no Cancelled."));
            }
        }

        harness.FocusView();
        harness.SendKey(ComponentKey.Tab);
        if (IsWithinView(component, harness.FocusedElement))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Keyboard, "Tab did not move focus out of the component (only the host owns tab traversal)."));
        }
        else if (!ReferenceEquals(harness.FocusedElement, trailing) && harness.FocusedElement is null)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Keyboard, "Tab lost focus entirely instead of moving to the next control."));
        }

        return violations;
    }

    /// <summary>The first two named siblings of a panel, in visual order, or null when there is none.</summary>
    private static Control[]? MirroredRow(ComponentHarness harness)
    {
        var panel = harness.Component.View.GetVisualDescendants()
            .OfType<Panel>()
            .FirstOrDefault(candidate => candidate.Children.Count >= 2
                && candidate.Children.All(child => child is Control { Name.Length: > 0 }));
        return panel is null ? null : new[] { panel.Children[0], panel.Children[1] };
    }

    private static bool IsWithinView(IComponent component, IInputElement? element)
    {
        if (element is null)
        {
            return false;
        }

        if (ReferenceEquals(element, component.View))
        {
            return true;
        }

        return element is Visual visual && component.View.IsVisualAncestorOf(visual);
    }
}
