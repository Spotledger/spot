using Spotledger.Component.Contract;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// A schema keyword the parser does not enforce is refused with its JSON pointer, at any depth,
/// instead of being dropped silently: a component must never believe it validated something it
/// did not.
/// </summary>
public sealed class PropsSchemaKeywordTests
{
    private static readonly string[] Declared = { "n", "mode", "caption", "rows" };

    // Each value is a JSON fragment: a valid use of the keyword in a real schema.
    public static TheoryData<string, string> Keywords => new()
    {
        { "$ref", "\"#/$defs/a\"" },
        { "oneOf", "[{\"type\":\"integer\"}]" },
        { "anyOf", "[{\"type\":\"integer\"}]" },
        { "allOf", "[{\"type\":\"integer\"}]" },
        { "not", "{\"type\":\"integer\"}" },
        { "patternProperties", "{\"^a\":{\"type\":\"integer\"}}" },
        { "if", "{\"type\":\"integer\"}" },
        { "then", "{\"type\":\"integer\"}" },
        { "else", "{\"type\":\"integer\"}" },
        { "format", "\"email\"" },
        { "pattern", "\"^a\"" },
        { "$defs", "{}" },
        { "minLength", "1" },
        { "exclusiveMinimum", "1" },
    };

    private static IReadOnlyList<string> Problems(string json)
    {
        Assert.False(PropsSchema.TryParse(json, out var schema, out var problems));
        Assert.Null(schema);
        return problems;
    }

    [Theory]
    [MemberData(nameof(Keywords))]
    public void AKeywordAtTheRootIsRefused(string keyword, string value)
    {
        var json = "{\"type\":\"object\",\"properties\":{\"a\":{\"type\":\"integer\"}}," + Quote(keyword) + ":" + value + "}";

        Assert.Contains(
            "props.schema.unsupported_keyword: '" + keyword + "' at /" + keyword,
            Problems(json).Single(p => p.Contains("unsupported_keyword", StringComparison.Ordinal)),
            StringComparison.Ordinal);
    }

    [Theory]
    [MemberData(nameof(Keywords))]
    public void AKeywordInAPropertyIsRefused(string keyword, string value)
    {
        var json = "{\"type\":\"object\",\"properties\":{\"a\":{\"type\":\"integer\"," + Quote(keyword) + ":" + value + "}}}";

        Assert.Contains(
            "'" + keyword + "' at /properties/a/" + keyword,
            Assert.Single(Problems(json), p => p.Contains("unsupported_keyword", StringComparison.Ordinal)),
            StringComparison.Ordinal);
    }

    [Theory]
    [MemberData(nameof(Keywords))]
    public void AKeywordInItemsIsRefused(string keyword, string value)
    {
        var json = "{\"type\":\"object\",\"properties\":{\"rows\":{\"type\":\"array\",\"items\":{\"type\":\"object\","
            + Quote(keyword) + ":" + value + "}}}}";

        Assert.Contains(
            "'" + keyword + "' at /properties/rows/items/" + keyword,
            Assert.Single(Problems(json), p => p.Contains("unsupported_keyword", StringComparison.Ordinal)),
            StringComparison.Ordinal);
    }

    [Fact]
    public void EveryUnsupportedKeywordIsReportedNotJustTheFirst()
    {
        var json = "{\"type\":\"object\",\"format\":\"x\",\"properties\":{\"a\":{\"type\":\"integer\",\"pattern\":\"x\",\"not\":{}}}}";

        Assert.Equal(3, Problems(json).Count(p => p.Contains("unsupported_keyword", StringComparison.Ordinal)));
    }

    [Fact]
    public void ThePointerEscapesSlashAndTilde()
    {
        var json = "{\"type\":\"object\",\"properties\":{\"a/b~c\":{\"type\":\"integer\",\"format\":\"x\"}}}";

        Assert.Contains("/properties/a~1b~0c/format", Assert.Single(Problems(json)), StringComparison.Ordinal);
    }

    [Fact]
    public void APropertyNamedLikeAKeywordIsAPropNotAKeyword()
    {
        const string json = "{\"type\":\"object\",\"properties\":{\"oneOf\":{\"type\":\"integer\"},\"format\":{\"type\":\"string\"}}}";

        Assert.True(PropsSchema.TryParse(json, out var schema, out var problems));
        Assert.Empty(problems);
        Assert.Equal(2, schema!.Properties.Count);
    }

    [Fact]
    public void AdditionalPropertiesIsAcceptedOnlyAsFalse()
    {
        const string accepted = "{\"type\":\"object\",\"additionalProperties\":false,\"properties\":{\"a\":{\"type\":\"integer\"}}}";
        const string refused = "{\"type\":\"object\",\"additionalProperties\":true,\"properties\":{\"a\":{\"type\":\"integer\"}}}";

        Assert.True(PropsSchema.TryParse(accepted, out _, out _));
        Assert.Contains("additionalProperties", Assert.Single(Problems(refused)), StringComparison.Ordinal);
    }

    [Fact]
    public void ASupportedSchemaIsUnchanged()
    {
        const string json = """
            {
              "$schema": "https://json-schema.org/draft/2020-12/schema",
              "title": "T", "description": "D", "default": {},
              "type": "object",
              "properties": {
                "n": { "type": "integer", "x-field-type": "int", "minimum": 1, "maximum": 5, "title": "N", "description": "d", "default": 2 },
                "mode": { "type": "string", "x-field-type": "select", "enum": ["a", "b"] },
                "caption": { "type": "string", "maxLength": 4 },
                "rows": { "type": "array", "items": { "type": "object", "title": "row" } }
              },
              "required": ["n"],
              "additionalProperties": false
            }
            """;

        Assert.True(PropsSchema.TryParse(json, out var schema, out var problems));
        Assert.Empty(problems);
        Assert.Equal(Declared, schema!.Properties.Select(p => p.Name));
    }

    [Fact]
    public void ParsingNeverThrowsOnDeepNesting()
    {
        var open = string.Concat(Enumerable.Repeat("{\"type\":\"array\",\"items\":", 40));
        var json = "{\"type\":\"object\",\"properties\":{\"a\":" + open + "{\"type\":\"integer\"}" + new string('}', 40) + "}}";

        var parsed = PropsSchema.TryParse(json, out _, out var problems);

        Assert.True(parsed || problems.Count > 0);
    }

    [Fact]
    public void AnUnsupportedKeywordBelowTheWalkedDepthIsRefusedNotIgnored()
    {
        var open = string.Concat(Enumerable.Repeat("{\"type\":\"array\",\"items\":", 12));
        var json = "{\"type\":\"object\",\"properties\":{\"a\":" + open + "{\"type\":\"integer\",\"pattern\":\"x\"}"
            + new string('}', 12) + "}}";

        Assert.False(PropsSchema.TryParse(json, out _, out var problems));
        Assert.Contains(problems, p => p.StartsWith("props.schema.too_deep", StringComparison.Ordinal));
    }

    private static string Quote(string keyword) => "\"" + keyword + "\"";
}
