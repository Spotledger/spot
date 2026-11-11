using System.Diagnostics;
using System.Runtime.CompilerServices;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.VisualTree;
using Spotledger.Component.Contract;

namespace Spotledger.Component.TestKit;

/// <summary>
/// The contract checks themselves, each returning the list of violations it found (empty means
/// the package honours the contract). The xUnit base class asserts these lists are empty; the
/// broken-variant tests assert they are not, which is what proves the checks bite.
/// </summary>
public static partial class ComponentContractKit
{
    /// <summary>Checks the descriptor and the declarations a live instance makes.</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckDescriptor(ComponentDescriptor descriptor)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = descriptor.Validate()
            .Select(problem => new ComponentContractViolation(ViolationCodes.Descriptor, problem))
            .ToList();
        if (violations.Count > 0 || descriptor.SupportedFieldTypes.Count == 0)
        {
            return violations;
        }

        using var component = descriptor.Create(TestContexts.Create(descriptor, FirstFieldType(descriptor)));
        violations.AddRange(CheckDeclarations(component));
        return violations;
    }

    /// <summary>Checks the declarations only (accessibility, keyboard, performance).</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckDeclarations(IComponent component)
    {
        ArgumentNullException.ThrowIfNull(component);
        var violations = new List<ComponentContractViolation>();
        violations.AddRange(component.Accessibility.Validate()
            .Select(problem => new ComponentContractViolation(ViolationCodes.Accessibility, problem)));
        violations.AddRange(component.Keyboard.Validate()
            .Select(problem => new ComponentContractViolation(ViolationCodes.Keyboard, problem)));
        violations.AddRange(component.Performance.Validate()
            .Select(problem => new ComponentContractViolation(ViolationCodes.Performance, problem)));
        if (component.View is null)
        {
            violations.Add(new ComponentContractViolation(ViolationCodes.Descriptor, "View is null: a component always renders a control."));
        }

        return violations;
    }

    /// <summary>Checks that the props schema parses, binds only supported types and validates prop sets.</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckPropsSchema(ComponentDescriptor descriptor)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = new List<ComponentContractViolation>();
        if (!PropsSchema.TryParse(descriptor.PropsSchemaJson, out var schema, out var problems))
        {
            violations.AddRange(problems.Select(p => new ComponentContractViolation(ViolationCodes.PropsSchema, p)));
            return violations;
        }

        violations.AddRange(schema!.ValidateAgainstSupportedTypes(descriptor.SupportedFieldTypes)
            .Select(p => new ComponentContractViolation(ViolationCodes.PropsSchema, p)));

        var missing = schema.Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal));
        foreach (var property in schema.Properties.Where(p => p.Required))
        {
            if (!missing.Any(m => m.Code == "props.required" && m.Message.Contains("'" + property.Name + "'", StringComparison.Ordinal)))
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.PropsSchema,
                    "required prop '" + property.Name + "' is not reported when it is absent."));
            }
        }

        var complete = new Dictionary<string, ComponentValue>(StringComparer.Ordinal);
        foreach (var property in schema.Properties)
        {
            complete[property.Name] = SampleFor(property);
        }

        foreach (var message in schema.Validate(complete))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.PropsSchema,
                "a complete, valid prop set was declined: " + message.Code + " " + message.Message));
        }

        return violations;
    }

    /// <summary>
    /// Round-trips a canonical, a null and an empty value through every supported field type,
    /// and checks the host-side <c>SetValue</c> never raises the user-driven event.
    /// </summary>
    public static IReadOnlyList<ComponentContractViolation> CheckValueRoundTrip(IComponent component)
    {
        ArgumentNullException.ThrowIfNull(component);
        var violations = new List<ComponentContractViolation>();
        var userChanges = 0;
        void OnChanged(object? sender, ComponentValueChangedEventArgs args) => userChanges++;
        component.ValueChanged += OnChanged;
        try
        {
            foreach (var type in component.Descriptor.SupportedFieldTypes)
            {
                var canonical = SampleValues.Canonical(type);
                component.SetValue(canonical);
                ComponentHarness.Pump();
                if (component.Value != canonical)
                {
                    violations.Add(new ComponentContractViolation(
                        ViolationCodes.ValueRoundTrip,
                        type.ToWireName() + ": SetValue(" + canonical + ") came back as " + component.Value + "."));
                }

                component.SetValue(ComponentValue.Null(type));
                ComponentHarness.Pump();
                if (!component.Value.IsNull)
                {
                    violations.Add(new ComponentContractViolation(
                        ViolationCodes.ValueRoundTrip,
                        type.ToWireName() + ": a null value came back as " + component.Value + "."));
                }

                var empty = SampleValues.Empty(type);
                component.SetValue(empty);
                ComponentHarness.Pump();
                if (component.Value != empty)
                {
                    violations.Add(new ComponentContractViolation(
                        ViolationCodes.ValueRoundTrip,
                        type.ToWireName() + ": the empty value came back as " + component.Value + "."));
                }
            }
        }
        finally
        {
            component.ValueChanged -= OnChanged;
        }

        if (userChanges != 0)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.ValueRoundTrip,
                "SetValue raised ValueChanged " + userChanges.ToString(System.Globalization.CultureInfo.InvariantCulture)
                + " time(s); ValueChanged is for user edits only, or the host loops."));
        }

        return violations;
    }

    /// <summary>Checks a read-only component: shown, enabled, and not changed by the keyboard.</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckReadOnly(ComponentDescriptor descriptor, ComponentFieldType type)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = new List<ComponentContractViolation>();
        var context = TestContexts.Create(descriptor, type, readOnly: true);
        using var harness = ComponentHarness.Show(descriptor, context);
        var before = harness.Component.Value;
        var changes = 0;
        harness.Component.ValueChanged += (_, _) => changes++;

        if (!harness.Component.View.IsEffectivelyEnabled)
        {
            violations.Add(new ComponentContractViolation(ViolationCodes.ReadOnly, "a read-only component is shown enabled, not disabled."));
        }

        harness.FocusView();
        foreach (var key in harness.Component.Keyboard.ActivationKeys)
        {
            harness.SendKey(key);
        }

        if (changes != 0 || harness.Component.Value != before)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.ReadOnly,
                "a read-only component changed from " + before + " to " + harness.Component.Value + " on a key press."));
        }

        return violations;
    }

    /// <summary>Checks a disabled component is actually disabled.</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckDisabled(ComponentDescriptor descriptor, ComponentFieldType type)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var context = TestContexts.Create(descriptor, type, disabled: true);
        using var harness = ComponentHarness.Show(descriptor, context);
        var violations = new List<ComponentContractViolation>();
        if (harness.Component.View.IsEffectivelyEnabled)
        {
            violations.Add(new ComponentContractViolation(ViolationCodes.Disabled, "a disabled component's view is still enabled."));
        }

        var before = harness.Component.Value;
        harness.FocusView();
        foreach (var key in harness.Component.Keyboard.ActivationKeys)
        {
            harness.SendKey(key);
        }

        if (harness.Component.Value != before)
        {
            violations.Add(new ComponentContractViolation(ViolationCodes.Disabled, "a disabled component still changed its value."));
        }

        return violations;
    }

    /// <summary>
    /// Checks validation: the hook refuses an empty required value and an out-of-range value,
    /// accepts a good one, raises <c>Validated</c>, and surfaces the message to assistive
    /// technology (help text plus a live region).
    /// </summary>
    public static IReadOnlyList<ComponentContractViolation> CheckValidation(ComponentDescriptor descriptor, ComponentFieldType type)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = new List<ComponentContractViolation>();
        using var harness = ComponentHarness.Show(descriptor, TestContexts.Create(descriptor, type));
        var component = harness.Component;
        ComponentValidatedEventArgs? last = null;
        component.Validated += (_, args) => last = args;

        var good = SampleValues.Canonical(type);
        var goodMessages = component.Validate(good);
        foreach (var message in goodMessages.Where(m => m.IsError))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Validation, "a valid value was refused: " + message.Code + " " + message.Message));
        }

        var emptyErrors = component.Validate(ComponentValue.Null(type)).Where(m => m.IsError).ToList();
        if (emptyErrors.Count == 0)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Validation, "a required field accepted the null value without an error."));
        }

        component.SetValue(ComponentValue.Null(type));
        ComponentHarness.Pump();
        if (last is null)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Validation, "Validated was not raised when the value became invalid."));
        }
        else
        {
            if (!last.HasErrors)
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.Validation, "Validated reported no errors for the null value of a required field."));
            }

            if (emptyErrors.Count > 0 && !last.Messages.Any(m => m.Code == emptyErrors[0].Code))
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.Validation, "the message raised differs from the one Validate returns."));
            }
        }

        var help = AutomationProperties.GetHelpText(component.View);
        if (string.IsNullOrWhiteSpace(help))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Validation, "the validation message is not surfaced: AutomationProperties.HelpText is empty."));
        }

        if (component.Accessibility.LiveRegion == ComponentLiveRegion.None)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Validation, "a component that shows validation messages declares a live region."));
        }

        if (descriptor.PropsSchema is { } schema && SampleValues.OutOfRange(schema, type) is { } bad)
        {
            var badErrors = component.Validate(bad).Where(m => m.IsError).ToList();
            if (badErrors.Count == 0)
            {
                violations.Add(new ComponentContractViolation(
                    ViolationCodes.Validation, "an out-of-range value (" + bad + ") was accepted."));
            }
        }

        return violations;
    }

    /// <summary>Checks the ControlTheme: no literals, real token references, and a template that applies.</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckThemeUsage(ComponentDescriptor descriptor)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = new List<ComponentContractViolation>();
        var xaml = ThemeText.Read(descriptor.ThemeSource, out var failure);
        if (xaml is null)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.ThemeMissing, "the ControlTheme " + descriptor.ThemeSource + " could not be read: " + failure));
            return violations;
        }

        violations.AddRange(ThemeLiterals.Scan(xaml));

        var tokens = ThemeLiterals.ReferencedTokens(xaml);
        var themeTokens = tokens.Where(key => key.Contains('.', StringComparison.Ordinal)).ToList();
        if (themeTokens.Count < 3)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.TokenUsage,
                "the styles reference " + themeTokens.Count.ToString(System.Globalization.CultureInfo.InvariantCulture)
                + " design tokens; a native-looking component uses at least three (colour, space, font)."));
        }

        using var harness = ComponentHarness.Show(descriptor, TestContexts.Create(descriptor, FirstFieldType(descriptor)));
        var visuals = harness.Component.View.GetVisualChildren().ToList();
        if (visuals.Count == 0)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.ThemeMissing, "the view has no visual children after layout: its ControlTheme never applied."));
        }

        return violations;
    }

    /// <summary>Checks the accessibility peer: a name, the declared role and the live setting.</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckAccessibility(ComponentDescriptor descriptor, ComponentFieldType type)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var violations = new List<ComponentContractViolation>();
        using var harness = ComponentHarness.Show(descriptor, TestContexts.Create(descriptor, type));
        var component = harness.Component;
        harness.FocusView();

        var declaredName = AutomationProperties.GetName(component.View);
        if (string.IsNullOrWhiteSpace(declaredName))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Accessibility, "AutomationProperties.Name is empty on the view."));
        }

        if (!component.View.Focusable)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Accessibility, "the view is not focusable, so a screen reader cannot reach it."));
        }

        var peer = ControlAutomationPeer.CreatePeerForElement(component.View);
        if (peer is null)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Accessibility, "no automation peer was created for the view."));
            return violations;
        }

        var peerName = peer.GetName();
        if (string.IsNullOrWhiteSpace(peerName))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Accessibility, "the automation peer has no name."));
        }

        if (!string.IsNullOrWhiteSpace(declaredName) && !string.Equals(peerName, declaredName, StringComparison.Ordinal))
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Accessibility,
                "the peer announces '" + peerName + "' but the view declares '" + declaredName + "'."));
        }

        var expectedRole = RoleFor(component.Accessibility.Role);
        var actualRole = peer.GetAutomationControlType();
        if (actualRole != expectedRole)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Accessibility,
                "the peer announces " + actualRole + " but the descriptor declares " + component.Accessibility.Role
                + " (expected " + expectedRole + ")."));
        }

        return violations;
    }

    /// <summary>
    /// Checks the performance budget: 1,000 updates, each followed by a layout pump, within the
    /// declared per-update time; and the managed allocation one update may make.
    /// </summary>
    public static IReadOnlyList<ComponentContractViolation> CheckPerformance(IComponent component, int updates = 1000)
    {
        ArgumentNullException.ThrowIfNull(component);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(updates);

        var violations = new List<ComponentContractViolation>();
        var type = FirstFieldType(component.Descriptor);
        var first = SampleValues.Canonical(type);
        var second = Alternate(type, first);
        var contract = component.Performance;

        for (var warmup = 0; warmup < 50; warmup++)
        {
            component.SetValue(warmup % 2 == 0 ? first : second);
        }

        ComponentHarness.Pump();

        var stopwatch = Stopwatch.StartNew();
        for (var update = 0; update < updates; update++)
        {
            component.SetValue(update % 2 == 0 ? second : first);
            ComponentHarness.Pump();
        }

        stopwatch.Stop();
        var averageMilliseconds = stopwatch.Elapsed.TotalMilliseconds / updates;
        if (averageMilliseconds > contract.BudgetMillisecondsPerUpdate)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Performance,
                "an update took " + averageMilliseconds.ToString("0.000", System.Globalization.CultureInfo.InvariantCulture)
                + " ms on average, over the declared " + contract.BudgetMillisecondsPerUpdate + " ms."));
        }

        var current = first;
        var before = GC.GetAllocatedBytesForCurrentThread();
        for (var update = 0; update < updates; update++)
        {
            current = current == first ? second : first;
            component.SetValue(current);
        }

        var allocatedPerUpdate = (GC.GetAllocatedBytesForCurrentThread() - before) / updates;
        if (allocatedPerUpdate > contract.MaxBytesAllocatedPerUpdate)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Performance,
                "an update allocated " + allocatedPerUpdate.ToString(System.Globalization.CultureInfo.InvariantCulture)
                + " bytes on average, over the declared " + contract.MaxBytesAllocatedPerUpdate + "."));
        }

        return violations;
    }

    /// <summary>Checks that disposing a shown component releases it and its view.</summary>
    public static IReadOnlyList<ComponentContractViolation> CheckDisposal(ComponentDescriptor descriptor)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var (component, view) = CreateShowAndDispose(descriptor);
        for (var attempt = 0; attempt < 3; attempt++)
        {
            GC.Collect();
            GC.WaitForPendingFinalizers();
        }

        var violations = new List<ComponentContractViolation>();
        if (component.IsAlive)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Disposal, "the component is still alive after Dispose: something holds it (an event or static)."));
        }

        if (view.IsAlive)
        {
            violations.Add(new ComponentContractViolation(
                ViolationCodes.Disposal, "the view is still alive after Dispose: the window or a static still holds it."));
        }

        return violations;
    }

    /// <summary>The first field type the descriptor supports (the kit's default binding).</summary>
    public static ComponentFieldType FirstFieldType(ComponentDescriptor descriptor)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        if (descriptor.SupportedFieldTypes.Count == 0)
        {
            return ComponentFieldType.Text;
        }

        return descriptor.SupportedFieldTypes.First();
    }

    /// <summary>The role a peer is expected to announce for a declared contract role.</summary>
    public static AutomationControlType RoleFor(ComponentAutomationRole role) => role switch
    {
        ComponentAutomationRole.Button => AutomationControlType.Button,
        ComponentAutomationRole.Toggle => AutomationControlType.CheckBox,
        ComponentAutomationRole.Slider => AutomationControlType.Slider,
        ComponentAutomationRole.TextInput => AutomationControlType.Edit,
        ComponentAutomationRole.Combo => AutomationControlType.ComboBox,
        ComponentAutomationRole.List => AutomationControlType.List,
        ComponentAutomationRole.Table => AutomationControlType.Table,
        ComponentAutomationRole.Status => AutomationControlType.StatusBar,
        _ => AutomationControlType.Group,
    };

    private static ComponentValue Alternate(ComponentFieldType type, ComponentValue current) => type switch
    {
        ComponentFieldType.Boolean => current.TryGetBoolean(out var flag) && flag
            ? ComponentValue.FromBoolean(false)
            : ComponentValue.FromBoolean(true),
        ComponentFieldType.IntegerNumber => current.TryGetInteger(out var integer)
            ? ComponentValue.FromInteger(integer + 1)
            : ComponentValue.FromInteger(1),
        ComponentFieldType.DecimalNumber => current.TryGetDecimal(out var number)
            ? ComponentValue.FromDecimal(number + 1)
            : ComponentValue.FromDecimal(1),
        _ => SampleValues.Empty(type),
    };

    private static ComponentValue SampleFor(PropsSchemaProperty property)
    {
        var type = property.FieldType;
        if (property.AllowedValues is { Count: > 0 } allowed)
        {
            return ComponentValue.FromString(type, allowed[0]);
        }

        if (type == ComponentFieldType.IntegerNumber)
        {
            return ComponentValue.FromInteger(property.Minimum is { } minimum ? (long)minimum : 1);
        }

        if (type == ComponentFieldType.DecimalNumber)
        {
            return ComponentValue.FromDecimal(property.Minimum ?? 1m);
        }

        var sample = SampleValues.Canonical(type);
        if (property.MaxLength is { } maxLength && sample.Text is { } text && text.Length > maxLength)
        {
            return ComponentValue.FromString(type, text[..maxLength]);
        }

        return sample;
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static (WeakReference Component, WeakReference View) CreateShowAndDispose(ComponentDescriptor descriptor)
    {
        var component = descriptor.Create(TestContexts.Create(descriptor, FirstFieldType(descriptor)));
        var view = component.View;
        using (var harness = ComponentHarness.ShowComponent(component))
        {
            harness.FocusView();
            ComponentHarness.Pump();
        }

        return (new WeakReference(component), new WeakReference(view));
    }
}
