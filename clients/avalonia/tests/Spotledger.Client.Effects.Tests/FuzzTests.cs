using System.Globalization;
using System.Text;
using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Effects.Tests;

/// <summary>
/// A malformed or hostile batch may cost a diagnostic, never an exception and never the form.
/// The generator draws shapes from the protocol's own member pool — wrong types, missing
/// members, absurd paths, truncated JSON — so the reader and applier are exercised with what a
/// broken server, a proxy or a bit flip could actually deliver.
/// </summary>
public class FuzzTests
{
    private const int Iterations = 400;

    private static readonly string[] Kinds = EffectsProtocol.AllKinds.ToArray();

    private static readonly string[] Paths =
    {
        "qty", "note", "title", "amount", "status", "customer", "items", "items[1].qty", "items[9].qty",
        "items[1].note", "nope", "items[0].qty", "items[1]", "a.b", "bad path", string.Empty, "items[1].line_total",
    };

    private static readonly string[] ScalarValues =
    {
        "null", "0", "1", "-5", "1.234", "\"12.50\"", "\"text\"", "\"open\"", "\"archived\"", "true", "false",
        "[]", "{}", "\"2026-10-21\"", "\"not a date\"",
    };

    /// <summary>Random batches never throw, and a refused batch is reported as refused.</summary>
    [Fact]
    public void RandomBatchesNeverThrow()
    {
        var random = new Random(20261021);
        for (int iteration = 0; iteration < Iterations; iteration++)
        {
            string body = RandomBatch(random);
            EffectReadResult read = EffectWire.Read(body);
            Assert.NotNull(read.Batch);
            Assert.NotNull(read.Problems);
            if (!read.Applied)
            {
                Assert.NotEmpty(read.Problems);
            }

            FormState form = TestForm.Load(withRow: true);
            ApplyResult result = new EffectApplier().Apply(form, read.Batch);
            Assert.NotNull(result);
            Assert.NotNull(result.Problems);
            Assert.NotNull(result.AppliedKinds);
            Assert.NotNull(result.DirtyPaths);
        }
    }

    /// <summary>Truncated and corrupted JSON is a refused batch, never an exception.</summary>
    [Fact]
    public void CorruptedJsonNeverThrows()
    {
        const string Valid = """
            {"version":1,"effects":[
              {"kind":"set_value","path":"qty","value":5},
              {"kind":"set_options","path":"status","options":[{"value":"open","label":"Open"}]},
              {"kind":"add_action","id":"pick","label":"Pick","effect":{"kind":"focus","path":"qty"}}
            ]}
            """;

        var random = new Random(7);
        for (int iteration = 0; iteration < Iterations; iteration++)
        {
            int cut = random.Next(Valid.Length);
            EffectReadResult truncated = EffectWire.Read(Valid[..cut]);
            Assert.NotNull(truncated.Batch);

            // A byte flip inside the document is either readable JSON (some other batch) or a
            // refusal; both are answers, neither is an exception.
            int at = random.Next(Valid.Length);
            var builder = new StringBuilder(Valid);
            builder[at] = "{}[]\",:01aZ \n"[random.Next(13)];
            EffectReadResult flipped = EffectWire.Read(builder.ToString());
            Assert.NotNull(flipped.Batch);

            FormState form = TestForm.Load();
            Assert.NotNull(new EffectApplier().Apply(form, truncated.Batch));
            Assert.NotNull(new EffectApplier().Apply(form, flipped.Batch));
        }
    }

    /// <summary>Random bytes that are not JSON are a refused batch with a diagnostic.</summary>
    [Fact]
    public void NonJsonBodiesAreRefusedNotThrown()
    {
        var random = new Random(11);
        for (int iteration = 0; iteration < 100; iteration++)
        {
            byte[] bytes = new byte[random.Next(0, 64)];
            random.NextBytes(bytes);
            string body = Encoding.UTF8.GetString(bytes);

            EffectReadResult read = EffectWire.Read(body);
            Assert.False(read.Applied);
            Assert.NotEmpty(read.Problems);
            Assert.True(read.Batch.IsEmpty);
        }

        EffectReadResult missing = EffectWire.Read("null");
        Assert.False(missing.Applied);
        Assert.Equal(EffectCodes.EffectInvalid, Assert.Single(missing.Problems).Code);
    }

    /// <summary>A deep batch is bounded, not a stack overflow: the reader refuses what is nested
    /// past its reading depth and the applier stops at the protocol's own cap.</summary>
    [Fact]
    public void DeeplyNestedBatchesAreBounded()
    {
        var builder = new StringBuilder();
        for (int depth = 0; depth < 200; depth++)
        {
            builder.Append("""{"kind":"batch","effects":[""");
        }

        builder.Append("""{"kind":"focus","path":"qty"}""");
        for (int depth = 0; depth < 200; depth++)
        {
            builder.Append("]}");
        }

        string body = string.Create(CultureInfo.InvariantCulture, $"{{\"version\":1,\"effects\":[{builder}]}}");
        EffectReadResult read = EffectWire.Read(body);
        Assert.True(read.Batch.IsEmpty, "a batch nested past the reading depth is refused, not unwound");
        Assert.Contains(read.Problems, problem => problem.Code == EffectCodes.EffectInvalid);

        // The applier's own cap, reached through the model (the reader never produces this).
        Effect nested = new FocusEffect("qty");
        for (int depth = 0; depth <= EffectsProtocol.MaxBatchDepth + 1; depth++)
        {
            nested = new BatchEffect(Array.AsReadOnly(new[] { nested }));
        }

        FormState form = TestForm.Load();
        ApplyResult result = new EffectApplier().Apply(form, EffectBatch.Of(nested));
        Assert.Contains(result.Problems, problem => problem.Code == EffectCodes.LimitExceeded);
        Assert.Null(form.Focus);
    }

    /// <summary>The applier is deterministic: the same random batch twice gives the same answer.</summary>
    [Fact]
    public void RandomBatchesApplyTheSameWayTwice()
    {
        var random = new Random(99);
        for (int iteration = 0; iteration < 100; iteration++)
        {
            string body = RandomBatch(random);
            EffectReadResult read = EffectWire.Read(body);

            FormState first = TestForm.Load(withRow: true);
            FormState second = TestForm.Load(withRow: true);
            ApplyResult a = new EffectApplier().Apply(first, read.Batch);
            ApplyResult b = new EffectApplier().Apply(second, read.Batch);

            Assert.Equal(a.AppliedKinds, b.AppliedKinds);
            Assert.Equal(a.Problems.Count, b.Problems.Count);
            Assert.Equal(a.ChangedPaths, b.ChangedPaths);
        }
    }

    private static string RandomBatch(Random random)
    {
        int count = random.Next(0, 8);
        var effects = new List<string>(count);
        for (int i = 0; i < count; i++)
        {
            effects.Add(RandomEffect(random));
        }

        string body = string.Join(',', effects);
        return string.Create(CultureInfo.InvariantCulture, $"{{\"version\":{random.Next(0, 3)},\"effects\":[{body}]}}");
    }

    private static string RandomEffect(Random random)
    {
        string kind = Kinds[random.Next(Kinds.Length)];
        string path = JsonText(Paths[random.Next(Paths.Length)]);
        string scalar = ScalarValues[random.Next(ScalarValues.Length)];
        return kind switch
        {
            "set_value" => $"{{\"kind\":\"set_value\",{Maybe(random, "path", path)}{Maybe(random, "value", scalar)}}}",
            "set_read_only" => $"{{\"kind\":\"set_read_only\",{Maybe(random, "path", path)}{Maybe(random, "read_only", scalar)}}}",
            "set_hidden" => $"{{\"kind\":\"set_hidden\",{Maybe(random, "path", path)}{Maybe(random, "hidden", scalar)}}}",
            "set_required" => $"{{\"kind\":\"set_required\",{Maybe(random, "path", path)}{Maybe(random, "required", scalar)}}}",
            "set_options" => $"{{\"kind\":\"set_options\",{Maybe(random, "path", path)}{Maybe(random, "options", Options(random))}}}",
            "set_label" => $"{{\"kind\":\"set_label\",{Maybe(random, "path", path)}{Maybe(random, "label", scalar)}}}",
            "add_action" => $"{{\"kind\":\"add_action\",{Maybe(random, "id", path)}{Maybe(random, "label", scalar)}{Maybe(random, "effect", RandomEffect(random))}}}",
            "remove_action" => $"{{\"kind\":\"remove_action\",{Maybe(random, "id", path)}}}",
            "show_message" => $"{{\"kind\":\"show_message\",{Maybe(random, "level", JsonText(random.Next(2) == 0 ? "info" : "loud"))}{Maybe(random, "text", scalar)}{Maybe(random, "field", path)}}}",
            "open_dialog" => $"{{\"kind\":\"open_dialog\",{Maybe(random, "dialog_id", path)}{Maybe(random, "params", scalar)}}}",
            "focus" => $"{{\"kind\":\"focus\",{Maybe(random, "path", path)}}}",
            "set_filter" => $"{{\"kind\":\"set_filter\",{Maybe(random, "link_path", path)}{Maybe(random, "filter", JsonText("doc.status =="))}}}",
            _ => $"{{\"kind\":\"batch\",{Maybe(random, "effects", "[" + RandomEffect(random) + "]")}}}",
        };
    }

    private static string Options(Random random)
    {
        int count = random.Next(0, 3);
        var options = new List<string>(count);
        for (int i = 0; i < count; i++)
        {
            string value = random.Next(3) switch
            {
                0 => "{\"value\":\"open\"}",
                1 => "{\"value\":\"draft\",\"label\":\"Draft\"}",
                _ => "{\"label\":\"no value\"}",
            };
            options.Add(value);
        }

        return "[" + string.Join(',', options) + "]";
    }

    /// <summary>A member, present most of the time so batches stay interesting.</summary>
    private static string Maybe(Random random, string name, string value) =>
        random.Next(5) == 0 ? string.Empty : $"{JsonText(name)}:{value},";

    private static string JsonText(string text)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStringValue(text);
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }
}
