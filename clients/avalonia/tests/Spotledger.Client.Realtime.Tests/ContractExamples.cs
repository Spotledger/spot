using System.Text;
using System.Text.Json;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>
/// Loads the examples of <c>RealtimeClientMessage</c> and <c>RealtimeServerMessage</c> from
/// <c>contracts/native-api/openapi.yaml</c>, so the tests use the contract's own vectors.
/// A change to the contract's examples fails these tests until the codec follows.
/// </summary>
internal static class ContractExamples
{
    private const string ClientSchema = "    RealtimeClientMessage:";
    private const string ServerSchema = "    RealtimeServerMessage:";
    private const string ExamplesHeader = "      examples:";

    private static readonly Lazy<IReadOnlyList<string>> Client = new(() => Load(ClientSchema));
    private static readonly Lazy<IReadOnlyList<string>> Server = new(() => Load(ServerSchema));

    /// <summary>The <c>RealtimeClientMessage</c> examples as JSON text, in contract order.</summary>
    public static IReadOnlyList<string> ClientMessages => Client.Value;

    /// <summary>The <c>RealtimeServerMessage</c> examples as JSON text, in contract order.</summary>
    public static IReadOnlyList<string> ServerMessages => Server.Value;

    /// <summary>The contract file, found by walking up from the test's output directory.</summary>
    public static string ContractPath
    {
        get
        {
            var directory = new DirectoryInfo(AppContext.BaseDirectory);
            while (directory is not null)
            {
                var candidate = Path.Combine(directory.FullName, "contracts", "native-api", "openapi.yaml");
                if (File.Exists(candidate))
                {
                    return candidate;
                }

                directory = directory.Parent;
            }

            throw new FileNotFoundException("Could not find contracts/native-api/openapi.yaml above the test output directory.");
        }
    }

    private static IReadOnlyList<string> Load(string schemaHeader)
    {
        var lines = File.ReadAllLines(ContractPath);
        var start = Array.IndexOf(lines, schemaHeader);
        if (start < 0)
        {
            throw new InvalidOperationException($"The contract no longer declares {schemaHeader.Trim()}.");
        }

        var index = start;
        while (index < lines.Length && lines[index] != ExamplesHeader)
        {
            index++;
        }

        if (index >= lines.Length)
        {
            throw new InvalidOperationException($"{schemaHeader.Trim()} has no examples in the contract.");
        }

        var examples = new List<IReadOnlyList<string>>();
        List<string>? current = null;
        for (index++; index < lines.Length; index++)
        {
            var line = lines[index];
            if (line.StartsWith("        - ", StringComparison.Ordinal))
            {
                current = [line[10..].TrimEnd()];
                examples.Add(current);
            }
            else if (line.StartsWith("          ", StringComparison.Ordinal) && line.Trim().Length > 0)
            {
                current?.Add(line.Trim());
            }
            else
            {
                break;
            }
        }

        return [.. examples.Select(ToJson)];
    }

    private static string ToJson(IReadOnlyList<string> lines)
    {
        if (lines[0].StartsWith('{'))
        {
            return FlowObjectToJson(lines[0]);
        }

        var pairs = lines.Select(SplitPair).ToArray();
        return "{" + string.Join(",", pairs.Select(pair => $"{Quote(pair.Key)}:{ValueToJson(pair.Value)}")) + "}";
    }

    private static string FlowObjectToJson(string flow)
    {
        var body = flow.Trim();
        if (!body.StartsWith('{') || !body.EndsWith('}'))
        {
            throw new InvalidOperationException($"Not a YAML flow mapping: {flow}");
        }

        body = body[1..^1];
        var parts = SplitTopLevel(body);
        var pairs = parts.Select(SplitPair).ToArray();
        return "{" + string.Join(",", pairs.Select(pair => $"{Quote(pair.Key)}:{ValueToJson(pair.Value)}")) + "}";
    }

    private static List<string> SplitTopLevel(string body)
    {
        var parts = new List<string>();
        var depth = 0;
        var inQuotes = false;
        var start = 0;
        for (var i = 0; i < body.Length; i++)
        {
            var c = body[i];
            if (c == '"')
            {
                inQuotes = !inQuotes;
            }
            else if (!inQuotes && c == '{')
            {
                depth++;
            }
            else if (!inQuotes && c == '}')
            {
                depth--;
            }
            else if (!inQuotes && depth == 0 && c == ',')
            {
                parts.Add(body[start..i]);
                start = i + 1;
            }
        }

        parts.Add(body[start..]);
        return [.. parts.Select(static part => part.Trim()).Where(static part => part.Length > 0)];
    }

    private static (string Key, string Value) SplitPair(string text)
    {
        var inQuotes = false;
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (c == '"')
            {
                inQuotes = !inQuotes;
            }
            else if (!inQuotes && c == ':')
            {
                return (text[..i].Trim(), text[(i + 1)..].Trim());
            }
        }

        throw new InvalidOperationException($"Not a YAML key/value pair: {text}");
    }

    private static string ValueToJson(string value)
    {
        if (value.StartsWith('{'))
        {
            return FlowObjectToJson(value);
        }

        if (value.StartsWith('"') || value.StartsWith('['))
        {
            return value;
        }

        // YAML integers and booleans are JSON scalars; every other bare scalar is a string.
        if (long.TryParse(value, System.Globalization.CultureInfo.InvariantCulture, out _)
            || value is "true" or "false" or "null")
        {
            return value;
        }

        return Quote(value);
    }

    private static string Quote(string value) => JsonSerializer.Serialize(value);
}
