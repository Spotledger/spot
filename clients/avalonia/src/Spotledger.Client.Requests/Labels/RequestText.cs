using Avalonia.Media;

namespace Spotledger.Client.Requests;

/// <summary>
/// The screen texts of this project. A text has placeholders (<c>{run}</c>, <c>{runs}</c>, ...);
/// the run label fills the first two, so no text spells the name of a run. Texts come from the
/// embedded resource of the locale, falling back to its language and then to English.
/// </summary>
public sealed class RequestText
{
    private static readonly HashSet<string> RightToLeftLanguages = new(StringComparer.OrdinalIgnoreCase) { "ar", "he", "fa", "ur" };
    private readonly List<Dictionary<string, string>> _chain;

    /// <summary>Creates the texts for <paramref name="locale"/> and <paramref name="label"/>.</summary>
    public RequestText(string locale, RunLabel label)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(locale);
        Locale = locale;
        Label = label ?? throw new ArgumentNullException(nameof(label));
        _chain = RunLabelResolver.Fallbacks(locale).Select(ResourceCatalogue.LoadTexts).ToList();
    }

    /// <summary>The locale the texts are in.</summary>
    public string Locale { get; }

    /// <summary>The run label every text uses.</summary>
    public RunLabel Label { get; }

    /// <summary>True when the language is written right to left.</summary>
    public bool RightToLeft
    {
        get
        {
            var dash = Locale.IndexOf('-', StringComparison.Ordinal);
            return RightToLeftLanguages.Contains(dash > 0 ? Locale[..dash] : Locale);
        }
    }

    /// <summary>The flow direction views set on their root.</summary>
    public FlowDirection FlowDirection => RightToLeft ? FlowDirection.RightToLeft : FlowDirection.LeftToRight;

    /// <summary>The text of <paramref name="key"/> with <paramref name="values"/> (pairs of placeholder name and value) filled in.</summary>
    public string Get(string key, params (string Name, string Value)[] values)
    {
        var text = key;
        foreach (var texts in _chain)
        {
            if (texts.TryGetValue(key, out var found))
            {
                text = found;
                break;
            }
        }

        text = text.Replace("{run}", Label.Singular, StringComparison.Ordinal)
            .Replace("{runs}", Label.Plural, StringComparison.Ordinal);
        foreach (var (name, value) in values)
        {
            text = text.Replace("{" + name + "}", value, StringComparison.Ordinal);
        }

        return text;
    }
}
