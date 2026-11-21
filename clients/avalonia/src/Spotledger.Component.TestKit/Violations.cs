using Avalonia;
using Spotledger.Component.Contract;

namespace Spotledger.Component.TestKit;

/// <summary>Stable codes every contract failure carries, so a broken package says what it broke.</summary>
public static class ViolationCodes
{
    /// <summary>The descriptor itself is unsound.</summary>
    public const string Descriptor = "descriptor.";

    /// <summary>The props schema is unsound or binds an unsupported type.</summary>
    public const string PropsSchema = "props.schema.";

    /// <summary>A value did not survive the round trip.</summary>
    public const string ValueRoundTrip = "value.round_trip.";

    /// <summary>A read-only or disabled component still let the user change the value.</summary>
    public const string ReadOnly = "value.read_only.";

    /// <summary>A disabled component was not disabled.</summary>
    public const string Disabled = "value.disabled.";

    /// <summary>Validation did not fire, or its messages were not surfaced.</summary>
    public const string Validation = "validation.";

    /// <summary>A hard-coded colour, font or metric was found in the component's styles.</summary>
    public const string TokenLiteral = "tokens.literal.";

    /// <summary>The component references too few design tokens to be styled by the theme.</summary>
    public const string TokenUsage = "tokens.unused.";

    /// <summary>The ControlTheme never applied, so the control renders unstyled.</summary>
    public const string ThemeMissing = "tokens.theme.";

    /// <summary>Layout did not mirror under right-to-left.</summary>
    public const string Rtl = "rtl.mirror.";

    /// <summary>Urdu text did not render.</summary>
    public const string Urdu = "rtl.urdu.";

    /// <summary>The keyboard contract was not honoured.</summary>
    public const string Keyboard = "keyboard.";

    /// <summary>The accessibility tree lacked a name or announced the wrong role.</summary>
    public const string Accessibility = "accessibility.";

    /// <summary>An update broke the performance budget.</summary>
    public const string Performance = "performance.";

    /// <summary>
    /// The package references an assembly, type or native import outside the allowed table. A static
    /// review aid, not a sandbox and not proof of safety (ADR-081).
    /// </summary>
    public const string AssemblyReference = "assembly.reference.";

    /// <summary>Disposal left the component or its view alive.</summary>
    public const string Disposal = "disposal.";
}

/// <summary>One contract failure: a stable code and a sentence a component author can act on.</summary>
/// <param name="Code">The code group from <see cref="ViolationCodes"/>.</param>
/// <param name="Message">What failed, with the concrete value.</param>
public sealed record ComponentContractViolation(string Code, string Message)
{
    /// <inheritdoc />
    public override string ToString() => Code + ": " + Message;
}

/// <summary>
/// The canonical sample values the kit drives through a component: one typed value, the null
/// value and the empty value per field type. Kept in one place so the round-trip test is the same
/// for every package.
/// </summary>
public static class SampleValues
{
    /// <summary>A typical value of the type.</summary>
    public static ComponentValue Canonical(ComponentFieldType type) => type switch
    {
        ComponentFieldType.Boolean => ComponentValue.FromBoolean(true),
        ComponentFieldType.IntegerNumber => ComponentValue.FromInteger(3),
        ComponentFieldType.DecimalNumber => ComponentValue.FromDecimal(12.50m),
        ComponentFieldType.Text => ComponentValue.FromText("Acme"),
        ComponentFieldType.LongText => ComponentValue.FromLongText("first line\nsecond line"),
        ComponentFieldType.Date => ComponentValue.FromDate(new DateOnly(2026, 2, 28)),
        ComponentFieldType.Time => ComponentValue.FromTime(new TimeOnly(13, 45, 5)),
        ComponentFieldType.Datetime => ComponentValue.FromInstant(new DateTimeOffset(2026, 2, 28, 13, 45, 5, TimeSpan.Zero)),
        ComponentFieldType.Link => ComponentValue.FromLink("ITEM-0001", "Widget"),
        ComponentFieldType.Select => ComponentValue.FromString(ComponentFieldType.Select, "open"),
        ComponentFieldType.Json => ComponentValue.FromJson("{\"a\":1}"),
        ComponentFieldType.Bytes => ComponentValue.FromBytes(new byte[] { 1, 2, 3 }),
        ComponentFieldType.Attach => ComponentValue.FromString(ComponentFieldType.Attach, "file-0001"),
        ComponentFieldType.Secret => ComponentValue.FromString(ComponentFieldType.Secret, "set"),
        ComponentFieldType.Table => ComponentValue.FromString(ComponentFieldType.Table, "[{\"item\":\"A\"}]"),
        _ => throw new ArgumentOutOfRangeException(nameof(type), type, "No sample for this field type."),
    };

    /// <summary>A value of the type with nothing in it.</summary>
    public static ComponentValue Empty(ComponentFieldType type) => ComponentValue.Empty(type);

    /// <summary>The value with no value at all.</summary>
    public static ComponentValue Null(ComponentFieldType type) => ComponentValue.Null(type);

    /// <summary>A value the schema declines, or null when the schema constrains nothing.</summary>
    public static ComponentValue? OutOfRange(PropsSchema schema, ComponentFieldType type)
    {
        ArgumentNullException.ThrowIfNull(schema);
        var property = schema.Properties.FirstOrDefault(p => p.FieldType == type);
        if (property is null)
        {
            return null;
        }

        if (property.MaxLength is { } maxLength)
        {
            return ComponentValue.FromString(type, new string('x', maxLength + 1));
        }

        if (property.Maximum is { } maximum && type == ComponentFieldType.IntegerNumber)
        {
            return ComponentValue.FromInteger((long)maximum + 1);
        }

        if (property.Minimum is { } minimum && type == ComponentFieldType.IntegerNumber)
        {
            return ComponentValue.FromInteger((long)minimum - 1);
        }

        if (property.AllowedValues is { Count: > 0 })
        {
            return ComponentValue.FromString(type, "not-an-allowed-value");
        }

        return null;
    }
}

/// <summary>The Urdu label the kit binds when it checks RTL and the Urdu font path.</summary>
public static class UrduSamples
{
    /// <summary>"Rating": the label an Urdu screen shows.</summary>
    public const string RatingLabel = "درجہ بندی";

    /// <summary>"Not a whole number": a message an Urdu screen shows.</summary>
    public const string InvalidMessage = "مقدار درست نہیں ہے";
}
