using System;

namespace Spotledger.Client.FlowDesigner;

/// The run-label pair (ADR-080): the only source of the user-facing name of a run.
///
/// The internal and API term stays <c>case</c>; a person never reads it. Every string in this
/// project that names a run is built from <see cref="Singular"/> and <see cref="Plural"/>, which
/// default to "Request"/"Requests" and may be overridden per organisation (or by a pack or a
/// locale). The label is configuration data, so changing it needs no release.
public sealed class RunLabels
{

    public const string SingularKey = "bpm.run.singular";

    public const string PluralKey = "bpm.run.plural";

    public const string DefaultSingular = "Request";

    public const string DefaultPlural = "Requests";

    private RunLabels(string singular, string plural)
    {
        Singular = singular;
        Plural = plural;
    }

    public string Singular { get; }

    public string Plural { get; }

    public static RunLabels Defaults { get; } = new(DefaultSingular, DefaultPlural);

    public static RunLabels Resolve(string? singular, string? plural) =>
        new(ResolveOne(singular, SingularKey, DefaultSingular), ResolveOne(plural, PluralKey, DefaultPlural));

    public string SingularLower => Singular.ToLowerInvariant();

    public string PluralLower => Plural.ToLowerInvariant();

    private static string ResolveOne(string? value, string key, string fallback)
    {
        if (value is null)
        {
            return fallback;
        }

        if (string.IsNullOrWhiteSpace(value))
        {
            throw new ArgumentException("The label override for " + key + " is blank.", nameof(value));
        }

        return value.Trim();
    }
}
