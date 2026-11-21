using System.Globalization;
using System.Text;

namespace Spotledger.Component.Host;

/// <summary>
/// The one place a refusal gets text a package chose (a file name, a manifest member name, an
/// assembly name). A refusal is shown in the UI and written to logs before the signature has been
/// checked, so such text is cut and flattened to printable ASCII first: control characters, escape
/// sequences, line breaks and bidirectional marks cannot reach a screen or a log line, and a very
/// long path cannot flood one. A refusal never names the package directory.
/// </summary>
public static class ComponentRefusalText
{
    /// <summary>The most characters of package-supplied text a refusal shows.</summary>
    public const int MaxPathCharacters = 120;

    private const string Ellipsis = "...";

    /// <summary>
    /// Package-supplied text made safe to show: printable ASCII kept, every other character
    /// replaced by <c>?</c>, and anything beyond <see cref="MaxPathCharacters"/> cut and marked.
    /// </summary>
    /// <param name="value">The text the package supplied; null reads as empty.</param>
    public static string SafePath(string? value)
    {
        if (string.IsNullOrEmpty(value))
        {
            return string.Empty;
        }

        var cut = value.Length > MaxPathCharacters;
        var take = cut ? MaxPathCharacters - Ellipsis.Length : value.Length;
        var text = new StringBuilder(MaxPathCharacters);
        for (var i = 0; i < take; i++)
        {
            var c = value[i];
            text.Append(c is >= ' ' and <= '~' ? c : '?');
        }

        if (cut)
        {
            text.Append(Ellipsis);
        }

        return text.ToString();
    }

    /// <summary>A listed file as a refusal names it: its zero-based index and its safe relative path.</summary>
    /// <param name="index">The file's zero-based position in the manifest's file list.</param>
    /// <param name="path">The path as the manifest wrote it.</param>
    public static string FileRef(int index, string? path) =>
        "file #" + index.ToString(CultureInfo.InvariantCulture) + " ('" + SafePath(path) + "')";
}
