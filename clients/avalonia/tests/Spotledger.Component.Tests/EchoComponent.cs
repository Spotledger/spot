using Avalonia.Controls;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Tests;

/// <summary>
/// A trivial component that echoes whatever value it is given, for every field type. It exists so
/// the kit's per-field-type coverage is proven rather than assumed: if the round-trip check missed
/// a type, this component would not be driven through it.
/// </summary>
internal sealed class EchoComponent : IComponent
{
    private readonly TextBlock _view = new();
    private ComponentValue _value;

    public EchoComponent(ComponentDescriptor descriptor, ComponentContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        Descriptor = descriptor;
        _value = context.Value;
        _view.Text = context.Value.Text ?? string.Empty;
        _view.Focusable = true;
    }

    public ComponentDescriptor Descriptor { get; }

    public Control View => _view;

    public ComponentValue Value => _value;

    public ComponentAccessibility Accessibility => new("Echo", ComponentAutomationRole.TextInput);

    public KeyboardContract Keyboard => new(
        EntryKeys: new[] { ComponentKey.Tab },
        ExitKeys: new[] { ComponentKey.Tab },
        ActivationKeys: new[] { ComponentKey.Space },
        HandledKeys: new[] { ComponentKey.Tab, ComponentKey.Space });

    public PerformanceContract Performance => new(BudgetMillisecondsPerUpdate: 8, MaxBytesAllocatedPerUpdate: 8192);

    public event EventHandler<ComponentValidatedEventArgs>? Validated;

    // This test double has no user interaction of its own, so the user-driven events are
    // implemented explicitly: present for the contract, inert by design.
    event EventHandler<ComponentValueChangedEventArgs>? IComponent.ValueChanged
    {
        add { }
        remove { }
    }

    event EventHandler<ComponentCommittedEventArgs>? IComponent.Committed
    {
        add { }
        remove { }
    }

    event EventHandler<ComponentCancelledEventArgs>? IComponent.Cancelled
    {
        add { }
        remove { }
    }

    public IReadOnlyList<ValidationMessage> Validate(ComponentValue proposed) => Array.Empty<ValidationMessage>();

    public void SetValue(ComponentValue value)
    {
        _value = value;
        _view.Text = value.Text ?? string.Empty;
        Validated?.Invoke(this, new ComponentValidatedEventArgs(Array.Empty<ValidationMessage>()));
    }

    public void Dispose()
    {
        // Nothing to release: these test doubles hold no subscription.
    }
}

/// <summary>The echo component's descriptor: every field type the contract knows.</summary>
internal static class EchoComponentDescriptor
{
    public const string SchemaJson = """
        {
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "type": "object",
          "properties": {
            "echo": { "type": "integer", "x-field-type": "int" }
          },
          "required": ["echo"]
        }
        """;

    public static ComponentDescriptor Instance { get; } = Create();

    private static ComponentDescriptor Create()
    {
        ComponentDescriptor? descriptor = null;
        descriptor = new ComponentDescriptor(
            "spot.echo",
            "Echo",
            new ComponentVersion(0, 1, 0),
            "spot",
            ContractVersionRange.ForMajor(ComponentContract.Version),
            ComponentFieldTypes.All,
            SchemaJson,
            new Uri("avares://Spotledger.Component.Tests/Themes/BrokenRating.axaml"),
            context => new EchoComponent(descriptor!, context));
        return descriptor;
    }
}
