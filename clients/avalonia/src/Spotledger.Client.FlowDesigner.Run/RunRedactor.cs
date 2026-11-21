using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;

namespace Spotledger.Client.FlowDesigner.Run;

/// One credential the client knows the value of, and the label its marker carries. The label is
/// the server's grammar: lower-case letters, digits and underscores, at most twenty characters, so
/// a marker the client writes reads exactly like one the server wrote.
public sealed record RunSecret
{

    public const int MaxLabelLength = 20;

    private const int LabelBaseLength = 14;

    public RunSecret(string label, string value)
    {
        if (!IsLabel(label))
        {
            throw new ArgumentException("A redaction label must match [a-z0-9_]{1,20}: " + label, nameof(label));
        }

        if (string.IsNullOrEmpty(value))
        {
            throw new ArgumentException("A secret needs a value.", nameof(value));
        }

        Label = label;
        Value = value;
    }

    public string Label { get; }

    public string Value { get; }

    /// The label a credential reference derives: lower-case, non-matching characters replaced by an
    /// underscore, the first fourteen characters kept (the server trims for its uniqueness suffix).
    public static string DeriveLabel(string reference)
    {
        ArgumentNullException.ThrowIfNull(reference);
        StringBuilder builder = new(LabelBaseLength);
        foreach (char c in reference)
        {
            if (builder.Length == LabelBaseLength)
            {
                break;
            }

            char lower = char.ToLowerInvariant(c);
            bool ok = (lower >= 'a' && lower <= 'z') || (lower >= '0' && lower <= '9');
            builder.Append(ok ? lower : '_');
        }

        return builder.Length == 0 ? "secret" : builder.ToString();
    }

    public static bool IsLabel(string? label)
    {
        if (string.IsNullOrEmpty(label) || label.Length > MaxLabelLength)
        {
            return false;
        }

        foreach (char c in label)
        {
            bool ok = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_';
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }
}

/// The client's own redaction pass over everything a run's panes can show (ADR-080 rule 3, and the
/// canary requirement of this task): the server redacts before it records, and the client redacts
/// again before it paints, so a value that reached the server by any route still never renders.
///
/// The rules are the server's, which is what makes the result comparable byte for byte:
/// - a sensitive key's value becomes the bare marker <c>[redacted]</c>;
/// - a known secret's value — and its base64 and URL-encoded forms — becomes
///   <c>[redacted:&lt;label&gt;]</c>;
/// - a value shorter than eight bytes cannot be matched safely (<c>spot-redact</c> refuses it), so
///   a payload that contains one is replaced by the bare marker instead of being printed;
/// - the pattern rules (private key block, JSON web token, bearer token, authorization header, API
///   key, parameter credential, connection-string credential, card number, IBAN) replace the value
///   and keep the key, scheme or header name where that reads better.
///
/// Every marker is inert: no rule's value class admits '[' and no known value can be a marker, so
/// redacting twice gives exactly the same text.
public sealed class RunRedactor
{

    /// The marker a sensitive key's value carries.
    public const string Marker = "[redacted]";

    /// The opening half of a labelled marker.
    public const string MarkerPrefix = "[redacted:";

    /// spot-redact refuses to register a value shorter than this, because matching it would collide
    /// with ordinary text; such a value is never printed instead.
    public const int MinimumMatchLength = 8;

    /// The built-in sensitive JSON keys, exactly the server's list (spot-redact, DEFAULT_FIELD_NAMES).
    private static readonly string[] SensitiveKeyNames =
    [
        "password",
        "secret",
        "token",
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "set-cookie",
        "private_key",
        "client_secret",
        "access_token",
        "refresh_token",
        "session_token",
        "auth_token",
        "recovery_code",
        "card_number",
    ];

    private static readonly TimeSpan MatchTimeout = TimeSpan.FromMilliseconds(200);

    private static readonly Regex PrivateKeyBlock = new(
        "-----BEGIN [A-Z ]*PRIVATE KEY-----[\\s\\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        RegexOptions.None,
        MatchTimeout);

    private static readonly Regex JsonWebToken = new(
        "\\beyJ[A-Za-z0-9_-]{4,}\\.[A-Za-z0-9_-]{4,}\\.[A-Za-z0-9_-]{4,}\\b",
        RegexOptions.None,
        MatchTimeout);

    private static readonly Regex BearerToken = new(
        "(?<=\\bBearer\\s)[A-Za-z0-9\\-._~+/]{8,}={0,2}",
        RegexOptions.None,
        MatchTimeout);

    private static readonly Regex HeaderValue = new(
        "\\b(Authorization|Proxy-Authorization|Cookie|Set-Cookie|X-Api-Key)\\s*:\\s*[^\\r\\n\"'\\[;]{4,}",
        RegexOptions.None,
        MatchTimeout);

    private static readonly Regex PlatformApiKey = new(
        "\\bspk_[A-Za-z0-9]{6,}_[A-Za-z0-9]{6,}\\b",
        RegexOptions.None,
        MatchTimeout);

    private static readonly Regex ParameterCredential = new(
        "\\b(password|passwd|secret|token|api_key|apikey|client_secret|access_token|refresh_token|session_token|recovery_code)\\s*=\\s*[^&\\s\"';,\\[]+",
        RegexOptions.IgnoreCase,
        MatchTimeout);

    private static readonly Regex ConnectionCredential = new(
        "\\b([a-z][a-z0-9+.\\-]*://)[^/\\s:@\\[]+:[^@\\s/\\[]+@",
        RegexOptions.IgnoreCase,
        MatchTimeout);

    private static readonly Regex CardNumber = new(
        "\\b(?:\\d[ -]?){12,18}\\d\\b",
        RegexOptions.None,
        MatchTimeout);

    private static readonly Regex Iban = new(
        "\\b[A-Z]{2}\\d{2}[A-Z0-9]{10,30}\\b",
        RegexOptions.None,
        MatchTimeout);

    private readonly List<RunSecret> _secrets;

    public RunRedactor(IEnumerable<RunSecret>? secrets = null)
    {
        _secrets = secrets is null ? [] : [.. secrets];
    }

    /// The sensitive keys, for a pane that wants to explain why a value is a marker.
    public static IReadOnlyList<string> SensitiveKeys => SensitiveKeyNames;

    /// The secrets this pass knows the values of.
    public IReadOnlyList<RunSecret> Secrets => _secrets;

    /// A redactor that also knows one more value.
    public RunRedactor With(RunSecret secret)
    {
        ArgumentNullException.ThrowIfNull(secret);
        return new RunRedactor([.. _secrets, secret]);
    }

    /// Whether a value is long enough to be matched safely; a shorter one can only be withheld.
    public static bool CanMatch(string? value) =>
        value is not null && Encoding.UTF8.GetByteCount(value) >= MinimumMatchLength;

    /// Whether a key's value is always a marker.
    public static bool IsSensitiveKey(string? key)
    {
        if (string.IsNullOrEmpty(key))
        {
            return false;
        }

        foreach (string name in SensitiveKeyNames)
        {
            if (string.Equals(name, key, StringComparison.OrdinalIgnoreCase))
            {
                return true;
            }
        }

        return false;
    }

    /// Whether a value is already a redaction marker, so a second pass keeps the server's label
    /// instead of replacing it with the bare one.
    public static bool IsMarker(string? text) =>
        text is not null
        && (string.Equals(text, Marker, StringComparison.Ordinal)
            || (text.StartsWith(MarkerPrefix, StringComparison.Ordinal) && text.EndsWith(']')));

    /// Whether a value node is already a marker, so a second pass keeps the server's label.
    public static bool IsMarkerNode(JsonTreeNode? node) =>
        node is { Kind: JsonKind.Text } && IsMarker(node.Text);

    /// The marker for a label.
    public static string MarkerFor(string label) => MarkerPrefix + label + "]";

    /// Redacts a string: pattern rules first, then every known value, so a marker never feeds a
    /// rule and a value that a rule already removed is not replaced twice.
    public string Redact(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return string.Empty;
        }

        // A marker is inert by construction: no rule admits '[', so a second pass over one would
        // change nothing, but returning it unchanged keeps the server's label exactly as written.
        if (IsMarker(text))
        {
            return text;
        }

        // A secret too short to match safely cannot be scrubbed by substitution, so nothing of the
        // string is printed at all — the registry's redact_text_or_placeholder behaviour.
        if (ContainsUnmatchableSecret(text))
        {
            return Marker;
        }

        try
        {
            string result = PrivateKeyBlock.Replace(text, MarkerFor("private_key"));
            result = JsonWebToken.Replace(result, MarkerFor("jwt"));
            result = HeaderValue.Replace(result, match => match.Groups[1].Value + ": " + MarkerFor("header"));
            result = BearerToken.Replace(result, MarkerFor("token"));
            result = PlatformApiKey.Replace(result, MarkerFor("api_key"));
            result = ParameterCredential.Replace(result, match => match.Groups[1].Value + "=" + MarkerFor("parameter"));
            result = ConnectionCredential.Replace(result, match => match.Groups[1].Value + MarkerFor("credentials") + "@");
            result = CardNumber.Replace(result, match => Luhn(match.Value) ? MarkerFor("card_number") : match.Value);
            result = Iban.Replace(result, match => Mod97(match.Value) ? MarkerFor("iban") : match.Value);

            foreach ((string value, string label) in KnownForms())
            {
                result = result.Replace(value, MarkerFor(label), StringComparison.Ordinal);
            }

            return result;
        }
        catch (RegexMatchTimeoutException)
        {
            // A pattern that cannot finish proves nothing about the text, so none of it is shown.
            return Marker;
        }
    }

    /// Redacts a string unless the value behind it could not be matched safely, in which case the
    /// whole string is withheld. This is the shape a pane uses for a value it cannot vouch for.
    public string RedactOrPlaceholder(string? text, RunSecret secret)
    {
        ArgumentNullException.ThrowIfNull(secret);
        return CanMatch(secret.Value) ? Redact(text) : Marker;
    }

    /// Whether the text still holds a value this redactor knows; a false answer means the pane must
    /// not be painted at all.
    public bool IsScrubbed(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return true;
        }

        foreach (RunSecret secret in _secrets)
        {
            if (text.Contains(secret.Value, StringComparison.Ordinal))
            {
                return false;
            }
        }

        return true;
    }

    /// The redacted copy of a payload: a sensitive key's value becomes the marker without being
    /// descended into, every string value is scrubbed, and markers already in the payload stay
    /// visible exactly as the server wrote them.
    public JsonTreeNode RedactTree(JsonTreeNode node)
    {
        ArgumentNullException.ThrowIfNull(node);
        switch (node.Kind)
        {
            case JsonKind.Map:
                JsonTreeNode map = JsonTree.Scalar(node.Name, JsonKind.Map, string.Empty);
                map.Expanded = node.Expanded;
                foreach (JsonTreeNode child in node.Children)
                {
                    if (IsSensitiveKey(child.Name) && !IsMarkerNode(child))
                    {
                        JsonTreeNode hidden = JsonTree.Scalar(child.Name, JsonKind.Text, Marker);
                        hidden.IsRedacted = true;
                        map.Add(hidden);
                    }
                    else
                    {
                        map.Add(RedactTree(child));
                    }
                }

                return map;
            case JsonKind.Sequence:
                JsonTreeNode list = JsonTree.Scalar(node.Name, JsonKind.Sequence, string.Empty);
                list.Expanded = node.Expanded;
                foreach (JsonTreeNode child in node.Children)
                {
                    list.Add(RedactTree(child));
                }

                return list;
            case JsonKind.Text:
                string clean = Redact(node.Text);
                JsonTreeNode scalar = JsonTree.Scalar(node.Name, JsonKind.Text, clean);
                scalar.IsRedacted = clean.StartsWith(MarkerPrefix, StringComparison.Ordinal)
                    || string.Equals(clean, Marker, StringComparison.Ordinal);
                return scalar;
            default:
                return JsonTree.Scalar(node.Name, node.Kind, node.Text);
        }
    }

    /// A payload as redacted text: parsed, scrubbed and written back. An unparseable payload is
    /// withheld rather than echoed, because the client cannot prove it holds no value.
    public string RedactJson(string? json)
    {
        if (string.IsNullOrWhiteSpace(json))
        {
            return string.Empty;
        }

        JsonParseResult parsed = JsonTree.Parse(json);
        return parsed.Root is { } root ? JsonTree.ToText(RedactTree(root)) : Marker;
    }

    /// The redacted tree of a payload, or null when it does not parse.
    public JsonTreeNode? RedactParsed(string? json)
    {
        if (string.IsNullOrWhiteSpace(json))
        {
            return null;
        }

        JsonParseResult parsed = JsonTree.Parse(json);
        return parsed.Root is { } root ? RedactTree(root) : null;
    }

    /// Every registered value and its encoded forms, longest first so a base64 form is removed
    /// before the plain text it contains.
    private List<(string Value, string Label)> KnownForms()
    {
        List<(string Value, string Label)> forms = [];
        foreach (RunSecret secret in _secrets)
        {
            if (!CanMatch(secret.Value))
            {
                continue;
            }

            forms.Add((secret.Value, secret.Label));
            string base64 = Convert.ToBase64String(Encoding.UTF8.GetBytes(secret.Value));
            if (!string.Equals(base64, secret.Value, StringComparison.Ordinal))
            {
                forms.Add((base64, secret.Label));
            }

            string encoded = Uri.EscapeDataString(secret.Value);
            if (!string.Equals(encoded, secret.Value, StringComparison.Ordinal)
                && !string.Equals(encoded, base64, StringComparison.Ordinal))
            {
                forms.Add((encoded, secret.Label));
            }
        }

        forms.Sort((left, right) => right.Value.Length.CompareTo(left.Value.Length));
        return forms;
    }

    private bool ContainsUnmatchableSecret(string text)
    {
        foreach (RunSecret secret in _secrets)
        {
            if (!CanMatch(secret.Value) && text.Contains(secret.Value, StringComparison.Ordinal))
            {
                return true;
            }
        }

        return false;
    }

    /// The Luhn check, integer-only, on the digits of a candidate card number.
    private static bool Luhn(string candidate)
    {
        string digits = Digits(candidate);
        if (digits.Length is < 13 or > 19)
        {
            return false;
        }

        int sum = 0;
        bool double_it = false;
        for (int i = digits.Length - 1; i >= 0; i--)
        {
            int digit = digits[i] - '0';
            if (double_it)
            {
                digit *= 2;
                if (digit > 9)
                {
                    digit -= 9;
                }
            }

            sum += digit;
            double_it = !double_it;
        }

        return sum % 10 == 0;
    }

    /// The mod-97 check for an IBAN, integer-only and without printing the value.
    private static bool Mod97(string candidate)
    {
        string compact = candidate.Replace(" ", string.Empty, StringComparison.Ordinal).ToUpperInvariant();
        if (compact.Length < 15 || compact.Length > 34)
        {
            return false;
        }

        string rearranged = compact[4..] + compact[..4];
        int remainder = 0;
        foreach (char c in rearranged)
        {
            if (c >= '0' && c <= '9')
            {
                remainder = ((remainder * 10) + (c - '0')) % 97;
            }
            else if (c >= 'A' && c <= 'Z')
            {
                int value = (c - 'A') + 10;
                remainder = ((remainder * 100) + value) % 97;
            }
            else
            {
                return false;
            }
        }

        return remainder == 1;
    }

    private static string Digits(string text)
    {
        StringBuilder builder = new(text.Length);
        foreach (char c in text)
        {
            if (c >= '0' && c <= '9')
            {
                builder.Append(c);
            }
        }

        return builder.ToString();
    }

    /// The digits of a number literal, for a test that wants to prove a card number is not echoed.
    public static string DigitString(string text) => Digits(text);

    public override string ToString() =>
        "RunRedactor(" + _secrets.Count.ToString(CultureInfo.InvariantCulture) + " secrets)";
}
