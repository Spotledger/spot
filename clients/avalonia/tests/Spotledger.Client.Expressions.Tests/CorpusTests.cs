using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Text.Json;
using Spotledger.Client.Expressions;
using Xunit;

namespace Spotledger.Client.Expressions.Tests;

/// <summary>
/// The conformance corpus of the Rust crate (<c>crates/framework/spot-expr/corpus/*.json</c>),
/// loaded from the repository by a relative path from the test assembly (the corpus files are
/// never copied). Every case must give the same result the Rust implementation gives: same
/// type, same value and, for decimals, same scale — or the same error code and offset.
/// </summary>
public class CorpusTests
{
    private static readonly Dictionary<string, JsonElement> Cases = Load();

    private static string CorpusDir()
    {
        // Walk up from the test assembly's directory to the repository root (the directory
        // that holds both clients/ and crates/).
        DirectoryInfo? dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null)
        {
            string candidate = Path.Combine(dir.FullName, "crates", "framework", "spot-expr", "corpus");
            if (Directory.Exists(candidate) && Directory.Exists(Path.Combine(dir.FullName, "clients")))
            {
                return candidate;
            }
            dir = dir.Parent;
        }
        throw new InvalidOperationException("the conformance corpus is not found relative to the test assembly");
    }

    private static Dictionary<string, JsonElement> Load()
    {
        Dictionary<string, JsonElement> cases = new(StringComparer.Ordinal);
        string[] files = Directory.GetFiles(CorpusDir(), "*.json");
        Array.Sort(files, StringComparer.Ordinal);
        foreach (string file in files)
        {
            using JsonDocument doc = JsonDocument.Parse(File.ReadAllText(file));
            string name = Path.GetFileName(file);
            foreach (JsonElement c in doc.RootElement.EnumerateArray())
            {
                cases.Add(name + "|" + (c.GetProperty("id").GetString() ?? "?"), c.Clone());
            }
        }
        return cases;
    }

    public static readonly TheoryData<string, string> AllCases = BuildTheory();

    private static TheoryData<string, string> BuildTheory()
    {
        TheoryData<string, string> data = new();
        List<string> keys = new(Cases.Keys);
        keys.Sort(StringComparer.Ordinal);
        foreach (string key in keys)
        {
            int bar = key.IndexOf('|');
            data.Add(key[..bar], key[(bar + 1)..]);
        }
        return data;
    }

    /// <summary>A typed value from the corpus ({"int": "42"} and the like).</summary>
    private static ExpressionValue ValueOf(JsonElement v)
    {
        foreach (JsonProperty property in v.EnumerateObject())
        {
            switch (property.Name)
            {
                case "null":
                    return ExpressionValue.Null;
                case "bool":
                    return new BoolValue(property.Value.GetBoolean());
                case "int":
                    return new IntValue(long.Parse(property.Value.GetString() ?? "0", CultureInfo.InvariantCulture));
                case "decimal":
                    return DecimalValueOf(property.Value.GetString() ?? "0");
                case "text":
                    return new TextValue(property.Value.GetString() ?? string.Empty);
                case "date":
                    return new DateValue(ExpressionDate.Parse(property.Value.GetString() ?? string.Empty)
                        ?? throw new InvalidOperationException($"bad corpus date {property.Value.GetString()}"));
                case "datetime":
                    return new DateTimeValue(ExpressionDateTime.Parse(property.Value.GetString() ?? string.Empty)
                        ?? throw new InvalidOperationException($"bad corpus datetime {property.Value.GetString()}"));
                case "list":
                    {
                        ExpressionValue[] items = new ExpressionValue[property.Value.GetArrayLength()];
                        int i = 0;
                        foreach (JsonElement item in property.Value.EnumerateArray())
                        {
                            items[i] = ValueOf(item);
                            i += 1;
                        }
                        return new ListValue(items);
                    }
            }
        }
        throw new InvalidOperationException("a corpus value has no known type key");
    }

    /// <summary>A corpus decimal string ("12.50", possibly signed) as a value: the scale counts.</summary>
    private static DecimalValue DecimalValueOf(string text)
    {
        bool negative = text.StartsWith('-');
        string body = negative || text.StartsWith('+') ? text[1..] : text;
        int dot = body.IndexOf('.');
        string intPart = dot >= 0 ? body[..dot] : body;
        string fracPart = dot >= 0 ? body[(dot + 1)..] : string.Empty;
        string digits = intPart + fracPart;
        if (digits.Length == 0 || digits.Length > 38)
        {
            throw new InvalidOperationException($"bad corpus decimal {text}");
        }
        Int128 mantissa = Int128.Parse(negative ? "-" + digits : digits, CultureInfo.InvariantCulture);
        return new DecimalValue(mantissa, fracPart.Length);
    }

    private static ExpressionMapContext ContextOf(JsonElement c, string now)
    {
        ExpressionDateTime at = ExpressionDateTime.Parse(now)
            ?? throw new InvalidOperationException($"bad corpus now {now}");
        ExpressionMapContext ctx = new(at);
        foreach (JsonProperty property in c.EnumerateObject())
        {
            ctx = ctx.With(property.Name, ValueOf(property.Value));
        }
        return ctx;
    }

    [Theory]
    [MemberData(nameof(AllCases))]
    public void Case(string file, string id)
    {
        JsonElement c = Cases[file + "|" + id];
        string source = c.GetProperty("expr").GetString() ?? string.Empty;
        string now = c.GetProperty("now").GetString() ?? string.Empty;
        JsonElement expect = c.GetProperty("expect");
        bool isError = expect.TryGetProperty("error", out JsonElement errorElement);
        string? expectedCode = isError ? errorElement.GetString() : null;
        int? expectedOffset = expect.TryGetProperty("offset", out JsonElement offsetElement) ? offsetElement.GetInt32() : null;

        if (!isError)
        {
            ExpressionValue expected = ValueOf(expect.GetProperty("ok"));
            ExpressionValue actual;
            try
            {
                actual = Expression.Evaluate(source, ContextOf(c.GetProperty("context"), now));
            }
            catch (Exception ex)
            {
                Assert.Fail($"{id}: expected {expected}, threw {ex.GetType().Name}: {ex.Message}");
                return;
            }
            Assert.True(
                expected.Same(actual),
                $"{id}: expected {expected}, got {actual}");
            return;
        }

        try
        {
            ExpressionValue actual = Expression.Evaluate(source, ContextOf(c.GetProperty("context"), now));
            Assert.Fail($"{id}: expected error {expectedCode}, got {actual}");
        }
        catch (ExpressionParseException ex)
        {
            Assert.True(ex.Code == expectedCode, $"{id}: expected error {expectedCode}, got parse error {ex.Code} ({ex.Message})");
            if (expectedOffset is int offset)
            {
                Assert.True(ex.Offset == offset, $"{id}: expected offset {offset}, got {ex.Offset}");
            }
        }
        catch (ExpressionEvaluationException ex)
        {
            Assert.True(ex.Code == expectedCode, $"{id}: expected error {expectedCode}, got evaluation error {ex.Code} ({ex.Message})");
        }
    }

    /// <summary>The whole corpus was loaded: no file or case silently missing.</summary>
    [Fact]
    public void CorpusIsComplete()
    {
        Assert.Equal(Files(), new HashSet<string>(CorpusFiles, StringComparer.Ordinal));
        Assert.Equal(ExpectedTotal, Cases.Count);
    }

    private static readonly string[] CorpusFiles =
    {
        "arithmetic.json", "comparison.json", "dates.json", "errors.json", "fields.json",
        "functions.json", "limits.json", "lists.json", "literals.json", "logic.json",
        "precedence.json", "text.json",
    };

    private static HashSet<string> Files()
    {
        HashSet<string> names = new(StringComparer.Ordinal);
        foreach (string key in Cases.Keys)
        {
            names.Add(key[..key.IndexOf('|')]);
        }
        return names;
    }

    private const int ExpectedTotal = 961;
}