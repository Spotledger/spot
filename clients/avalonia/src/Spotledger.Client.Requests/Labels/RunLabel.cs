using System.Reflection;
using System.Text.Json;

namespace Spotledger.Client.Requests;

/// <summary>The user-facing name of a run: a singular and a plural. It always comes from the label
/// catalogue (<c>bpm.run.singular</c> / <c>bpm.run.plural</c>), never from code.</summary>
public sealed record RunLabel(string Singular, string Plural)
{
    /// <summary>The catalogue key of the singular.</summary>
    public const string SingularKey = "bpm.run.singular";

    /// <summary>The catalogue key of the plural.</summary>
    public const string PluralKey = "bpm.run.plural";
}

/// <summary>The localisation catalogue as the client reads it: one text per key, locale and
/// (optionally) organisation. The server sends the catalogue with the session; a host wraps it here.</summary>
public interface IRunLabelCatalogue
{
    /// <summary>The text of <paramref name="key"/> for <paramref name="locale"/>, with the organisation's override when it has one; null when there is none.</summary>
    string? Text(string key, string locale, string? organisation);
}

/// <summary>A catalogue held in memory: the shipped one, a session's, or a test's.</summary>
public sealed class MemoryRunLabelCatalogue : IRunLabelCatalogue
{
    private readonly Dictionary<(string Key, string Locale, string Org), string> _texts = [];

    /// <summary>Sets a text for everyone (<paramref name="organisation"/> null) or for one organisation.</summary>
    public MemoryRunLabelCatalogue Set(string key, string locale, string? organisation, string text)
    {
        _texts[(key, locale, organisation ?? string.Empty)] = text;
        return this;
    }

    /// <inheritdoc />
    public string? Text(string key, string locale, string? organisation)
    {
        if (organisation is not null && _texts.TryGetValue((key, locale, organisation), out var own))
        {
            return own;
        }

        return _texts.TryGetValue((key, locale, string.Empty), out var shared) ? shared : null;
    }
}

/// <summary>Resolves the run label for a locale and organisation. A catalogue text wins, then the
/// shipped catalogue (an embedded resource); a locale falls back to its language, then to English.</summary>
public sealed class RunLabelResolver(IRunLabelCatalogue? session = null)
{
    private static readonly Lazy<IRunLabelCatalogue> Shipped = new(() => ResourceCatalogue.Load("labels.json"));

    /// <summary>The label for <paramref name="locale"/> (for example <c>ar-EG</c>) in <paramref name="organisation"/>.</summary>
    public RunLabel Resolve(string locale, string? organisation = null)
    {
        var singular = Find(RunLabel.SingularKey, locale, organisation);
        var plural = Find(RunLabel.PluralKey, locale, organisation);
        return new RunLabel(singular, plural);
    }

    private string Find(string key, string locale, string? organisation)
    {
        foreach (var candidate in Fallbacks(locale))
        {
            var text = session?.Text(key, candidate, organisation);
            if (!string.IsNullOrWhiteSpace(text))
            {
                return text;
            }
        }

        foreach (var candidate in Fallbacks(locale))
        {
            var text = Shipped.Value.Text(key, candidate, null);
            if (!string.IsNullOrWhiteSpace(text))
            {
                return text;
            }
        }

        // The shipped catalogue always has English; this is only reachable if the resource is lost.
        return key;
    }

    internal static IEnumerable<string> Fallbacks(string locale)
    {
        yield return locale;
        var dash = locale.IndexOf('-', StringComparison.Ordinal);
        if (dash > 0)
        {
            yield return locale[..dash];
        }

        yield return "en";
    }
}

/// <summary>Loads the catalogue embedded in this assembly (<c>Resources/labels.json</c>: locale to key to text).</summary>
internal static class ResourceCatalogue
{
    internal static IRunLabelCatalogue Load(string file)
    {
        var catalogue = new MemoryRunLabelCatalogue();
        using var stream = Assembly.GetExecutingAssembly()
            .GetManifestResourceStream("Spotledger.Client.Requests.Resources." + file);
        if (stream is null)
        {
            return catalogue;
        }

        using var document = JsonDocument.Parse(stream);
        foreach (var locale in document.RootElement.EnumerateObject())
        {
            foreach (var entry in locale.Value.EnumerateObject())
            {
                catalogue.Set(entry.Name, locale.Name, null, entry.Value.GetString() ?? string.Empty);
            }
        }

        return catalogue;
    }

    internal static Dictionary<string, string> LoadTexts(string locale)
    {
        var result = new Dictionary<string, string>(StringComparer.Ordinal);
        using var stream = Assembly.GetExecutingAssembly()
            .GetManifestResourceStream($"Spotledger.Client.Requests.Resources.texts.{locale}.json");
        if (stream is null)
        {
            return result;
        }

        using var document = JsonDocument.Parse(stream);
        foreach (var entry in document.RootElement.EnumerateObject())
        {
            result[entry.Name] = entry.Value.GetString() ?? string.Empty;
        }

        return result;
    }
}
