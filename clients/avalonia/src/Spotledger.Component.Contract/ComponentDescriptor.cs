using System.Globalization;
using System.Text.RegularExpressions;

namespace Spotledger.Component.Contract;

/// <summary>
/// Everything the host and the store need to know about a component before running it: identity,
/// version, publisher, the contract range it targets, the field types it can render, its props
/// schema and the ControlTheme resource that styles it. The factory is the only way to make an
/// instance, so constructing one is always paired with the context it was declared for.
/// </summary>
public sealed partial class ComponentDescriptor
{
    private readonly Func<ComponentContext, IComponent> _create;
    private PropsSchema? _propsSchema;

    /// <summary>Creates a descriptor. Call <see cref="Validate"/> before registering it.</summary>
    public ComponentDescriptor(
        string id,
        string displayName,
        ComponentVersion version,
        string publisher,
        ContractVersionRange contractRange,
        IReadOnlyCollection<ComponentFieldType> supportedFieldTypes,
        string propsSchemaJson,
        Uri themeSource,
        Func<ComponentContext, IComponent> create)
    {
        Id = id ?? throw new ArgumentNullException(nameof(id));
        DisplayName = displayName ?? throw new ArgumentNullException(nameof(displayName));
        Version = version;
        Publisher = publisher ?? throw new ArgumentNullException(nameof(publisher));
        ContractRange = contractRange ?? throw new ArgumentNullException(nameof(contractRange));
        SupportedFieldTypes = supportedFieldTypes ?? throw new ArgumentNullException(nameof(supportedFieldTypes));
        PropsSchemaJson = propsSchemaJson ?? throw new ArgumentNullException(nameof(propsSchemaJson));
        ThemeSource = themeSource ?? throw new ArgumentNullException(nameof(themeSource));
        _create = create ?? throw new ArgumentNullException(nameof(create));
    }

    /// <summary>The reverse-DNS id, for example <c>acme.rating</c>; unique per publisher namespace.</summary>
    public string Id { get; }

    /// <summary>The name a form slot shows in a picker.</summary>
    public string DisplayName { get; }

    /// <summary>The component's own version.</summary>
    public ComponentVersion Version { get; }

    /// <summary>The signing publisher's id.</summary>
    public string Publisher { get; }

    /// <summary>The contract versions this build supports.</summary>
    public ContractVersionRange ContractRange { get; }

    /// <summary>The field types this component can bind to.</summary>
    public IReadOnlyCollection<ComponentFieldType> SupportedFieldTypes { get; }

    /// <summary>The props schema, as shipped.</summary>
    public string PropsSchemaJson { get; }

    /// <summary>The ControlTheme resource that styles this component (an <c>avares://</c> URI).</summary>
    public Uri ThemeSource { get; }

    /// <summary>The parsed props schema; null when <see cref="PropsSchemaJson"/> is invalid.</summary>
    public PropsSchema? PropsSchema
    {
        get
        {
            if (_propsSchema is null && PropsSchema.TryParse(PropsSchemaJson, out var schema, out _))
            {
                _propsSchema = schema;
            }

            return _propsSchema;
        }
    }

    /// <summary>Creates an instance bound to <paramref name="context"/>.</summary>
    public IComponent Create(ComponentContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        return _create(context);
    }

    /// <summary>
    /// The assembly that holds this descriptor's factory, taken from the factory delegate. The kit's
    /// assembly-reference check reads its metadata (without running it). It is the assembly the
    /// author's factory lives in, so a package that builds its component elsewhere must put the
    /// factory in the component assembly. Null when the delegate has no declaring type.
    /// </summary>
    public System.Reflection.Assembly? FactoryAssembly => _create.Method.DeclaringType?.Assembly;

    /// <summary>True when this component claims the given field type.</summary>
    public bool Supports(ComponentFieldType fieldType) => SupportedFieldTypes.Contains(fieldType);

    /// <summary>
    /// Everything wrong with this descriptor; empty when it is sound. The kit fails a component
    /// whose descriptor reports a single problem, so this is the gate a package must clear first.
    /// </summary>
    public IReadOnlyList<string> Validate()
    {
        var problems = new List<string>();
        if (!IdPattern().IsMatch(Id))
        {
            problems.Add("descriptor.id: '" + Id + "' is not a dotted lowercase id such as 'acme.rating'.");
        }

        if (string.IsNullOrWhiteSpace(DisplayName) || DisplayName.Length > 64)
        {
            problems.Add("descriptor.display_name: DisplayName is 1 to 64 characters.");
        }

        if (string.IsNullOrWhiteSpace(Publisher) || !PublisherPattern().IsMatch(Publisher))
        {
            problems.Add("descriptor.publisher: Publisher is a dotted lowercase id such as 'acme'.");
        }

        if (SupportedFieldTypes.Count == 0)
        {
            problems.Add("descriptor.field_types: the descriptor supports no field type.");
        }

        if (SupportedFieldTypes.Distinct().Count() != SupportedFieldTypes.Count)
        {
            problems.Add("descriptor.field_types: a field type is listed twice.");
        }

        if (!ThemeSource.IsAbsoluteUri || !string.Equals(ThemeSource.Scheme, "avares", StringComparison.Ordinal))
        {
            problems.Add("descriptor.theme: ThemeSource is an avares:// URI inside the component's own assembly.");
        }

        if (!ContractCompatibilityRules(problems))
        {
            return problems;
        }

        if (!PropsSchema.TryParse(PropsSchemaJson, out var schema, out var schemaProblems))
        {
            problems.AddRange(schemaProblems);
            return problems;
        }

        problems.AddRange(schema!.ValidateAgainstSupportedTypes(SupportedFieldTypes));
        return problems;
    }

    /// <summary>The accessibility, keyboard and performance declarations of a live instance.</summary>
    internal static IReadOnlyList<string> ValidateInstance(IComponent component)
    {
        ArgumentNullException.ThrowIfNull(component);
        var problems = new List<string>();
        problems.AddRange(component.Accessibility.Validate());
        problems.AddRange(component.Keyboard.Validate());
        problems.AddRange(component.Performance.Validate());
        return problems;
    }

    private bool ContractCompatibilityRules(List<string> problems)
    {
        if (ContractRange.Minimum.Major != ContractRange.MaximumExclusive.Major - 1)
        {
            problems.Add("descriptor.contract: the contract range spans exactly one major.");
        }

        return problems.Count == 0;
    }

    /// <inheritdoc />
    public override string ToString() =>
        Id + "@" + Version.ToString() + " (" + ContractRange.ToString() + ")";

    [GeneratedRegex("^[a-z][a-z0-9_]*([.-][a-z][a-z0-9_]*)+$", RegexOptions.CultureInvariant)]
    private static partial Regex IdPattern();

    [GeneratedRegex("^[a-z][a-z0-9_]*$", RegexOptions.CultureInvariant)]
    private static partial Regex PublisherPattern();
}

/// <summary>The outcome of registering a descriptor: accepted, or a stable refusal code and message.</summary>
/// <param name="Accepted">True when the registry took the descriptor.</param>
/// <param name="Code">A stable refusal code such as <c>component.contract.too_new</c>; null when accepted.</param>
/// <param name="Message">A message a reviewer can act on; null when accepted.</param>
public sealed record ComponentRegistrationResult(bool Accepted, string? Code, string? Message)
{
    /// <summary>An acceptance.</summary>
    public static ComponentRegistrationResult Ok { get; } = new(true, null, null);

    /// <summary>A refusal.</summary>
    public static ComponentRegistrationResult Refused(string code, string message) => new(false, code, message);
}

/// <summary>
/// The host-side registry: it accepts a descriptor only when its contract range covers the
/// host's contract version, its descriptor is sound and its id is not already taken. Refusal is
/// how an incompatible range is stopped before any code runs.
/// </summary>
public sealed class ComponentRegistry
{
    private readonly Dictionary<string, ComponentDescriptor> _byId = new(StringComparer.Ordinal);
    private readonly List<ComponentDescriptor> _ordered = new();

    /// <summary>Creates a registry for the contract version this host speaks.</summary>
    public ComponentRegistry()
        : this(ComponentContract.Version)
    {
    }

    /// <summary>Creates a registry for an explicit host contract version (tests pin one).</summary>
    public ComponentRegistry(ContractVersion hostVersion)
    {
        HostVersion = hostVersion;
    }

    /// <summary>The contract version this registry accepts.</summary>
    public ContractVersion HostVersion { get; }

    /// <summary>The accepted descriptors, in registration order.</summary>
    public IReadOnlyList<ComponentDescriptor> Components => _ordered;

    /// <summary>Tries to register <paramref name="descriptor"/>, refusing anything incompatible.</summary>
    public ComponentRegistrationResult Register(ComponentDescriptor descriptor)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        if (!ComponentContract.TryAccept(descriptor.ContractRange, out var code, out var message)
            || !descriptor.ContractRange.Contains(HostVersion))
        {
            return ComponentRegistrationResult.Refused(
                code ?? "component.contract.unsupported",
                message ?? "Component targets contract " + descriptor.ContractRange.ToString()
                    + " but this host speaks " + HostVersion.ToString() + ".");
        }

        var problems = descriptor.Validate();
        if (problems.Count > 0)
        {
            return ComponentRegistrationResult.Refused("component.descriptor.invalid", string.Join(" ", problems));
        }

        if (_byId.ContainsKey(descriptor.Id))
        {
            return ComponentRegistrationResult.Refused(
                "component.id.duplicate",
                "A component with id '" + descriptor.Id + "' is already registered.");
        }

        _byId.Add(descriptor.Id, descriptor);
        _ordered.Add(descriptor);
        return ComponentRegistrationResult.Ok;
    }

    /// <summary>Looks a descriptor up by id.</summary>
    public bool TryResolve(string id, out ComponentDescriptor? descriptor)
    {
        descriptor = null;
        return id is not null && _byId.TryGetValue(id, out descriptor);
    }
}

/// <summary>
/// The host side of the contract: from the registry and the field a slot binds to, it creates a
/// component instance or refuses with a stable reason. This is the only supported way for a
/// shell to obtain an <see cref="IComponent"/>.
/// </summary>
public sealed class ComponentHost
{
    private readonly ComponentRegistry _registry;

    /// <summary>Creates a host over a registry.</summary>
    public ComponentHost(ComponentRegistry registry)
    {
        _registry = registry ?? throw new ArgumentNullException(nameof(registry));
    }

    /// <summary>The registry this host resolves from.</summary>
    public ComponentRegistry Registry => _registry;

    /// <summary>Tries to create a component for <paramref name="componentId"/> bound to <paramref name="context"/>.</summary>
    public bool TryCreate(string componentId, ComponentContext context, out IComponent? component, out string? refusal)
    {
        ArgumentNullException.ThrowIfNull(context);
        component = null;
        if (!_registry.TryResolve(componentId, out var descriptor) || descriptor is null)
        {
            refusal = "component.unknown: no component with id '" + componentId + "' is registered.";
            return false;
        }

        if (!descriptor.Supports(context.Field.Type))
        {
            refusal = "component.field_type.unsupported: " + descriptor.Id + " does not render "
                + context.Field.Type.ToWireName() + " fields.";
            return false;
        }

        IComponent? created = null;
        try
        {
            created = descriptor.Create(context);
            if (created is not null && created.View is null)
            {
                // A component without a control is half built: release it and refuse.
                DisposeQuietly(created);
                refusal = "component.create_failed: " + descriptor.Id + " could not be created.";
                return false;
            }
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            // Author code ran and failed: say which component, never the exception
            // message or stack (they can carry anything the author put in them). When the factory
            // handed back an instance before the failure (for example its View getter threw), that
            // instance is disposed; whatever a factory allocated and leaked before throwing is the
            // factory's own cleanup, which the host cannot reach.
            DisposeQuietly(created);
            refusal = "component.create_failed: " + descriptor.Id + " could not be created.";
            return false;
        }

        if (created is null)
        {
            refusal = "component.create_failed: " + descriptor.Id + " could not be created.";
            return false;
        }

        refusal = null;
        component = created;
        return true;
    }

    private static void DisposeQuietly(IComponent? component)
    {
        if (component is not IDisposable disposable)
        {
            return;
        }

        try
        {
            disposable.Dispose();
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            // Nothing more to do with a component that cannot even dispose.
        }
    }
}
