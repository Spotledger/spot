using Spotledger.Component.Contract;

namespace Acme.RatingComponent;

/// <summary>
/// Acme's published descriptor: what the store signs and the host reads before it renders the
/// rating. The props schema is the tier-B end of the same field vocabulary the metadata runtime
/// uses, so a form designer can offer this component for an integer field.
/// </summary>
public static class AcmeRatingDescriptor
{
    /// <summary>The component's identity.</summary>
    public const string ComponentId = "acme.rating";

    /// <summary>The publisher that signs it.</summary>
    public const string PublisherId = "acme";

    /// <summary>The ControlTheme resource inside this package's assembly.</summary>
    public static Uri ThemeSource { get; } = new("avares://Acme.RatingComponent/Themes/AcmeRating.axaml");

    /// <summary>The props schema: how many stars, and the lowest star that counts.</summary>
    public const string PropsSchemaJson = """
        {
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "title": "Acme star rating",
          "description": "A five-star rating bound to an integer field.",
          "type": "object",
          "properties": {
            "max": {
              "type": "integer",
              "x-field-type": "int",
              "minimum": 1,
              "maximum": 5,
              "description": "How many stars the control draws."
            },
            "min": {
              "type": "integer",
              "x-field-type": "int",
              "minimum": 1,
              "maximum": 5,
              "description": "The lowest rating the field accepts."
            }
          },
          "required": ["max"],
          "additionalProperties": false
        }
        """;

    /// <summary>The descriptor instance the package registers.</summary>
    public static ComponentDescriptor Instance { get; } = new(
        ComponentId,
        "Acme star rating",
        new ComponentVersion(1, 0, 0),
        PublisherId,
        ContractVersionRange.ForMajor(ComponentContract.Version),
        new[] { ComponentFieldType.IntegerNumber },
        PropsSchemaJson,
        ThemeSource,
        static context => new AcmeRatingComponent(context));
}
