using System.Globalization;
using Avalonia.Controls;
using Spotledger.Component.Contract;
using Spotledger.Component.Host;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// A component the tests own: it renders a border, keeps its value, and can be told to throw on an
/// update or raise a user edit. It lives in this assembly, so a slot can be exercised without a
/// package — which is exactly the case a host-built component has.
/// </summary>
public sealed class StubComponent : IComponent
{
    /// <summary>Creates the stub.</summary>
    /// <param name="descriptor">The descriptor that created it.</param>
    /// <param name="context">The context the host passed.</param>
    /// <param name="throwOnUpdate">True when <see cref="SetValue"/> must throw.</param>
    public StubComponent(ComponentDescriptor descriptor, ComponentContext context, bool throwOnUpdate)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        ArgumentNullException.ThrowIfNull(context);
        Descriptor = descriptor;
        ThrowOnUpdate = throwOnUpdate;
        Value = context.Value;
        View = new Border();
    }

    /// <inheritdoc />
    public ComponentDescriptor Descriptor { get; }

    /// <inheritdoc />
    public Control View { get; }

    /// <inheritdoc />
    public ComponentValue Value { get; private set; }

    /// <summary>True after <see cref="Dispose"/>.</summary>
    public bool IsDisposed { get; private set; }

    /// <summary>True when <see cref="Dispose"/> must throw: a component whose teardown fails.</summary>
    public bool ThrowOnDispose { get; set; }

    /// <summary>How many times the host set a value on this instance.</summary>
    public int Updates { get; private set; }

    /// <summary>True when <see cref="SetValue"/> throws.</summary>
    public bool ThrowOnUpdate { get; }

    /// <summary>The number of handlers a host still has subscribed to <see cref="ValueChanged"/>.</summary>
    public int ValueChangedSubscribers => ValueChanged?.GetInvocationList().Length ?? 0;

    /// <summary>
    /// The total number of handlers subscribed to any of the four events. An uninstall must take this
    /// to zero: a host handler left subscribed is a delegate into the package's code, and it is what
    /// keeps a collectible context alive.
    /// </summary>
    public int CallbackSubscribers =>
        (ValueChanged?.GetInvocationList().Length ?? 0)
        + (Committed?.GetInvocationList().Length ?? 0)
        + (Cancelled?.GetInvocationList().Length ?? 0)
        + (Validated?.GetInvocationList().Length ?? 0);

    /// <inheritdoc />
    public event EventHandler<ComponentValueChangedEventArgs>? ValueChanged;

    /// <inheritdoc />
    public event EventHandler<ComponentCommittedEventArgs>? Committed;

    /// <inheritdoc />
    public event EventHandler<ComponentCancelledEventArgs>? Cancelled;

    /// <inheritdoc />
    public event EventHandler<ComponentValidatedEventArgs>? Validated;

    /// <inheritdoc />
    public void SetValue(ComponentValue value)
    {
        if (ThrowOnUpdate)
        {
            throw new InvalidOperationException("the stub refuses this update.");
        }

        Updates++;
        Value = value;
    }

    /// <inheritdoc />
    public IReadOnlyList<ValidationMessage> Validate(ComponentValue proposed) => Array.Empty<ValidationMessage>();

    /// <inheritdoc />
    public ComponentAccessibility Accessibility { get; } = new("Stub", ComponentAutomationRole.TextInput);

    /// <inheritdoc />
    public KeyboardContract Keyboard { get; } = new(
        Array.Empty<ComponentKey>(),
        Array.Empty<ComponentKey>(),
        Array.Empty<ComponentKey>(),
        Array.Empty<ComponentKey>());

    /// <inheritdoc />
    public PerformanceContract Performance { get; } = new(8, 4096);

    /// <summary>Simulates a user edit, which is what the host's callback would receive.</summary>
    public void RaiseValueChanged()
    {
        var next = ComponentValue.FromInteger(3);
        var previous = Value;
        Value = next;
        ValueChanged?.Invoke(this, new ComponentValueChangedEventArgs(next, previous));
    }

    /// <inheritdoc />
    public void Dispose()
    {
        IsDisposed = true;
        ValueChanged = null;
        Committed = null;
        Cancelled = null;
        Validated = null;
        if (ThrowOnDispose)
        {
            throw new InvalidOperationException("the component's teardown failed");
        }
    }
}

/// <summary>The descriptors the tests build: a sound stub, a throwing one, and an integer schema.</summary>
public static class TestDescriptors
{
    /// <summary>A props schema with one integer prop, the shape the resolver validates against.</summary>
    public const string IntegerSchema = """
        {
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "title": "Stub",
          "type": "object",
          "properties": {
            "max": {
              "type": "integer",
              "x-field-type": "int",
              "minimum": 1,
              "maximum": 5
            }
          },
          "required": ["max"],
          "additionalProperties": false
        }
        """;

    /// <summary>A stub descriptor whose factory makes a <see cref="StubComponent"/>.</summary>
    /// <param name="id">The component id.</param>
    /// <param name="throwOnUpdate">True when the created component throws on an update.</param>
    public static ComponentDescriptor Stub(string id = "test.stub", bool throwOnUpdate = false)
    {
        ComponentDescriptor? descriptor = null;
        descriptor = new ComponentDescriptor(
            id,
            "Stub component",
            new ComponentVersion(1, 0, 0),
            "acme",
            ContractVersionRange.ForMajor(new ContractVersion(1, 0)),
            new[] { ComponentFieldType.IntegerNumber },
            IntegerSchema,
            new Uri("avares://Stub/Themes/Stub.axaml"),
            context => new StubComponent(descriptor!, context, throwOnUpdate));
        return descriptor;
    }

    /// <summary>A descriptor whose factory throws, for the failure-isolation tests.</summary>
    /// <param name="id">The component id.</param>
    public static ComponentDescriptor Throwing(string id = "test.throwing") =>
        new(
            id,
            "Throwing component",
            new ComponentVersion(1, 0, 0),
            "acme",
            ContractVersionRange.ForMajor(new ContractVersion(1, 0)),
            new[] { ComponentFieldType.IntegerNumber },
            IntegerSchema,
            new Uri("avares://Stub/Themes/Stub.axaml"),
            _ => throw new InvalidOperationException("the constructor throws."));

    /// <summary>A descriptor for a component that does not render the tested field type.</summary>
    public static ComponentDescriptor TextOnly(string id = "test.text") =>
        new(
            id,
            "Text-only component",
            new ComponentVersion(1, 0, 0),
            "acme",
            ContractVersionRange.ForMajor(new ContractVersion(1, 0)),
            new[] { ComponentFieldType.Text },
            """
            {
              "$schema": "https://json-schema.org/draft/2020-12/schema",
              "title": "Text stub",
              "type": "object",
              "properties": {
                "placeholder": {
                  "type": "string",
                  "x-field-type": "text",
                  "maxLength": 40
                }
              },
              "additionalProperties": false
            }
            """,
            new Uri("avares://Stub/Themes/Stub.axaml"),
            context => new StubComponent(TextOnly(id), context, throwOnUpdate: false));

    /// <summary>The context a slot opens with: an integer field holding 3.</summary>
    /// <param name="name">The field name.</param>
    public static ComponentContext Context(string name = "rating") =>
        new(ComponentValue.FromInteger(3), new FieldMetadata(name, "Rating", ComponentFieldType.IntegerNumber));

    /// <summary>An invariant rendering of an integer, for messages.</summary>
    /// <param name="value">The integer.</param>
    public static string Show(long value) => value.ToString(CultureInfo.InvariantCulture);
}

/// <summary>
/// A monotonic clock the test drives, so a budget overrun is a stated number and not a race. Each read
/// advances by <see cref="StepTicks"/>, so a measured call takes exactly one step.
/// </summary>
public sealed class FakeClock : IMonotonicClock
{
    private long _current;

    /// <summary>How far each read advances the clock.</summary>
    public long StepTicks { get; set; }

    /// <summary>The current tick, after advancing by one step.</summary>
    public long Timestamp
    {
        get
        {
            _current += StepTicks;
            return _current;
        }
    }
}

/// <summary>Records what a host logged, so a test can assert the refusal a user would have been shown.</summary>
public sealed class RecordingLog : IComponentHostLog
{
    /// <summary>Every refusal, in order.</summary>
    public List<ComponentRefusal> Refusals { get; } = new();

    /// <summary>Every component that loaded, as <c>id@version</c>.</summary>
    public List<string> LoadedIds { get; } = new();

    /// <summary>Every component that unloaded, as <c>id@version</c>.</summary>
    public List<string> UnloadedIds { get; } = new();

    /// <summary>Every run-time failure.</summary>
    public List<ComponentRefusal> Failures { get; } = new();

    /// <inheritdoc />
    public void Refused(ComponentRefusal refusal) => Refusals.Add(refusal);

    /// <inheritdoc />
    public void Loaded(LoadedComponent component) =>
        LoadedIds.Add(component.ComponentId + "@" + component.Descriptor.Version.ToString());

    /// <inheritdoc />
    public void Unloaded(string componentId, string version) => UnloadedIds.Add(componentId + "@" + version);

    /// <inheritdoc />
    public void Failed(ComponentRefusal refusal) => Failures.Add(refusal);
}
