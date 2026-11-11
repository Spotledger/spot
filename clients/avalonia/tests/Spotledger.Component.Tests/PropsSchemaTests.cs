using Acme.RatingComponent;
using Spotledger.Component.Contract;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// The props schema subset the contract enforces: it parses, binds prop names to field types, and
/// validates a prop set at run time — the same document a form designer reads.
/// </summary>
public sealed class PropsSchemaTests
{
    private static readonly ComponentFieldType[] IntegerOnly = { ComponentFieldType.IntegerNumber };

    private static readonly ComponentFieldType[] TextOnly = { ComponentFieldType.Text };

    private static readonly string[] PropNames = { "max", "min" };

    private static PropsSchema AcmeSchema()
    {
        Assert.True(PropsSchema.TryParse(AcmeRatingDescriptor.PropsSchemaJson, out var schema, out var problems));
        Assert.Empty(problems);
        return schema!;
    }

    [Fact]
    public void SchemaDeclaresItsProps()
    {
        var schema = AcmeSchema();

        Assert.Equal(PropNames, schema.Properties.Select(p => p.Name));
        Assert.All(schema.Properties, property => Assert.Equal(ComponentFieldType.IntegerNumber, property.FieldType));
        Assert.True(schema.Properties[0].Required);
        Assert.False(schema.Properties[1].Required);
        Assert.Equal(5m, schema.Properties[0].Maximum);
        Assert.Equal(1m, schema.Properties[0].Minimum);
    }

    [Fact]
    public void SchemaBindsOnlySupportedFieldTypes()
    {
        var schema = AcmeSchema();

        Assert.Empty(schema.ValidateAgainstSupportedTypes(IntegerOnly));

        var unsupported = schema.ValidateAgainstSupportedTypes(TextOnly);
        Assert.Equal(2, unsupported.Count);
        Assert.All(unsupported, problem => Assert.Contains("props.schema.unsupported", problem, StringComparison.Ordinal));
    }

    [Fact]
    public void MissingRequiredPropIsReported()
    {
        var messages = AcmeSchema().Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal));

        Assert.Contains(messages, message => message.Code == "props.required" && message.Message.Contains("'max'", StringComparison.Ordinal));
    }

    [Fact]
    public void CompletePropSetIsAccepted()
    {
        var messages = AcmeSchema().Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal)
        {
            ["max"] = ComponentValue.FromInteger(5),
            ["min"] = ComponentValue.FromInteger(1),
        });

        Assert.Empty(messages);
    }

    [Fact]
    public void OutOfRangeAndEnumViolationsAreReported()
    {
        var schema = AcmeSchema();

        var over = schema.Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal)
        {
            ["max"] = ComponentValue.FromInteger(9),
        });
        Assert.Contains(over, message => message.Code == "props.maximum");

        var under = schema.Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal)
        {
            ["max"] = ComponentValue.FromInteger(0),
        });
        Assert.Contains(under, message => message.Code == "props.minimum");
    }

    [Fact]
    public void UnknownAndMistypedPropsAreReported()
    {
        var schema = AcmeSchema();

        var unknown = schema.Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal)
        {
            ["max"] = ComponentValue.FromInteger(5),
            ["colour"] = ComponentValue.FromText("gold"),
        });
        Assert.Contains(unknown, message => message.Code == "props.unknown");

        var mistyped = schema.Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal)
        {
            ["max"] = ComponentValue.FromText("five"),
        });
        Assert.Contains(mistyped, message => message.Code == "props.type");
    }

    [Fact]
    public void EnumAndLengthConstraintsAreEnforced()
    {
        const string json = """
            {
              "$schema": "https://json-schema.org/draft/2020-12/schema",
              "type": "object",
              "properties": {
                "mode": { "type": "string", "x-field-type": "select", "enum": ["compact", "roomy"] },
                "caption": { "type": "string", "x-field-type": "text", "maxLength": 4 }
              },
              "required": ["mode"]
            }
            """;
        Assert.True(PropsSchema.TryParse(json, out var schema, out _));

        var bad = schema!.Validate(new Dictionary<string, ComponentValue>(StringComparer.Ordinal)
        {
            ["mode"] = ComponentValue.FromString(ComponentFieldType.Select, "huge"),
            ["caption"] = ComponentValue.FromText("too long"),
        });

        Assert.Contains(bad, message => message.Code == "props.enum");
        Assert.Contains(bad, message => message.Code == "props.max_length");
    }

    [Theory]
    [InlineData("{ not json")]
    [InlineData("[]")]
    [InlineData("""{"type":"array","properties":{"a":{"type":"integer"}}}""")]
    [InlineData("""{"$schema":"https://json-schema.org/draft/2019-09/schema","type":"object","properties":{"a":{"type":"integer"}}}""")]
    [InlineData("""{"type":"object","properties":{"a":{"type":"x-bogus"}}}""")]
    [InlineData("""{"type":"object","properties":{"a":{"type":"integer","x-field-type":"text"}}}""")]
    [InlineData("""{"type":"object","properties":{},"required":["ghost"]}""")]
    public void UnsoundSchemasAreRefused(string json)
    {
        Assert.False(PropsSchema.TryParse(json, out var schema, out var problems));
        Assert.Null(schema);
        Assert.NotEmpty(problems);
    }
}
