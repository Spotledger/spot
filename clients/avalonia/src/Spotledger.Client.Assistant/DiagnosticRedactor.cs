using System.Text;
using System.Text.RegularExpressions;

namespace Spotledger.Client.Assistant;

/// <summary>
/// Strips secrets from a diagnostic bundle before the user reviews it. The redaction is
/// deliberately blunt: anything that looks like a credential, a bearer token, a long
/// token-like string or an email address is replaced, because the user then reads exactly
/// what will be sent.
/// </summary>
public static partial class DiagnosticRedactor
{
    /// <summary>The marker that replaces a redacted value.</summary>
    public const string Marker = "[redacted]";

    /// <summary>Builds the bundle text from the chips and the last error, redacted.</summary>
    public static DiagnosticBundle Build(IEnumerable<ContextChip> chips, string? lastError)
    {
        var sb = new StringBuilder();
        foreach (var chip in chips)
        {
            sb.Append(chip.Kind).Append(": ").Append(chip.Value).Append('\n');
        }

        if (!string.IsNullOrWhiteSpace(lastError))
        {
            sb.Append("error: ").Append(lastError).Append('\n');
        }

        return new DiagnosticBundle(Redact(sb.ToString()));
    }

    /// <summary>Redacts secrets and personal addresses from <paramref name="text"/>.</summary>
    public static string Redact(string text)
    {
        var result = Bearer().Replace(text, "Bearer " + Marker);
        result = Credential().Replace(result, m => m.Groups[1].Value + Marker);
        result = Email().Replace(result, Marker);
        return LongToken().Replace(result, Marker);
    }

    [GeneratedRegex(@"(?i)\b((?:password|passwd|secret|token|api[_-]?key|authorization)\s*[=:]\s*)\S+")]
    private static partial Regex Credential();

    [GeneratedRegex(@"(?i)Bearer\s+[A-Za-z0-9._~+/=-]+")]
    private static partial Regex Bearer();

    [GeneratedRegex(@"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")]
    private static partial Regex Email();

    [GeneratedRegex(@"\b[A-Za-z0-9_-]{32,}\b")]
    private static partial Regex LongToken();
}
