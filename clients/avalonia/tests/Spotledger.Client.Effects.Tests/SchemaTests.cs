using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using Spotledger.Client.Effects.Tests.Schema;
using Xunit;

namespace Spotledger.Client.Effects.Tests;

/// <summary>
/// The JSON Schema and the C# model must not drift apart. The schema
/// (<c>crates/framework/spot-form-effects/schema/effects.v1.json</c>) is the contract the
/// server writes; this client reads it. The tests here compare the schema's own declarations
/// with <see cref="EffectSchema"/> member by member, then judge sample payloads in both
/// directions: JSON the server would write must read into the model, and payloads the model
/// writes must satisfy the schema.
/// </summary>
/// <remarks>
/// One asymmetry is deliberate and tested as such: the schema is closed
/// (<c>additionalProperties: false</c>) because the server must not write members v1 does not
/// define, while the client ignores an unknown member, which is the protocol's evolution
/// rule. A schema check refuses it; the reader keeps the effect.
/// </remarks>
public class SchemaTests
{
    private static readonly JsonDocument Schema = LoadSchema();

    private static readonly string[] EnvelopeRequired = { "effects", "version" };

    private static readonly IReadOnlyList<Sample> Samples = BuildSamples();

    /// <summary>Every effect kind the schema declares is in the model, with the same members.</summary>
    [Fact]
    public void TheSchemaAndTheModelAgree()
    {
        JsonElement defs = Schema.RootElement.GetProperty("$defs");
        var schemaKinds = new List<string>();
        foreach (JsonProperty definition in defs.EnumerateObject())
        {
            if (!definition.Value.TryGetProperty("properties", out JsonElement properties)
                || !properties.TryGetProperty("kind", out JsonElement kind)
                || !kind.TryGetProperty("const", out JsonElement constant)
                || constant.ValueKind != JsonValueKind.String)
            {
                continue;
            }

            string name = constant.GetString()!;
            schemaKinds.Add(name);
            EffectKindShape? shape = EffectSchema.For(name);
            Assert.True(shape is not null, $"the schema declares `{name}` and the model does not");

            Assert.True(
                definition.Value.TryGetProperty("additionalProperties", out JsonElement closed)
                    && closed.ValueKind == JsonValueKind.False,
                $"`{name}` must stay closed for the server");

            string[] required = definition.Value.GetProperty("required")
                .EnumerateArray().Select(item => item.GetString()!).OrderBy(text => text, StringComparer.Ordinal).ToArray();
            string[] modelRequired = shape!.RequiredMemberNames.OrderBy(text => text, StringComparer.Ordinal).ToArray();
            Assert.Equal(required, modelRequired);

            string[] schemaMembers = properties.EnumerateObject().Select(member => member.Name).OrderBy(text => text, StringComparer.Ordinal).ToArray();
            string[] modelMembers = shape.Members.Select(member => member.Name).OrderBy(text => text, StringComparer.Ordinal).ToArray();
            Assert.Equal(schemaMembers, modelMembers);

            foreach (EffectMemberShape member in shape.Members)
            {
                Assert.True(properties.TryGetProperty(member.Name, out JsonElement declared), $"{name}.{member.Name} is missing from the schema");
                string declaredType = DeclaredType(declared, defs);
                Assert.True(
                    string.Equals(member.Type, declaredType, StringComparison.Ordinal),
                    $"{name}.{member.Name}: the model says `{member.Type}`, the schema says `{declaredType}`");

            }

            var atLeastOne = new List<string>();
            if (definition.Value.TryGetProperty("anyOf", out JsonElement alternatives))
            {
                foreach (JsonElement alternative in alternatives.EnumerateArray())
                {
                    foreach (JsonElement member in alternative.GetProperty("required").EnumerateArray())
                    {
                        atLeastOne.Add(member.GetString()!);
                    }
                }
            }

            string[] expectedAlternatives = (shape.AtLeastOneOf ?? Array.Empty<string>()).OrderBy(text => text, StringComparer.Ordinal).ToArray();
            atLeastOne.Sort(StringComparer.Ordinal);
            Assert.Equal(expectedAlternatives, atLeastOne);
        }

        Assert.Equal(
            EffectsProtocol.AllKinds.OrderBy(text => text, StringComparer.Ordinal).ToArray(),
            schemaKinds.OrderBy(text => text, StringComparer.Ordinal).ToArray());
    }

    /// <summary>The envelope: v1 only, with the effects array the model reads.</summary>
    [Fact]
    public void TheBatchEnvelopeMatchesTheSchema()
    {
        JsonElement root = Schema.RootElement;
        Assert.Equal(EffectsProtocol.Version, root.GetProperty("properties").GetProperty("version").GetProperty("const").GetInt32());
        string[] required = root.GetProperty("required").EnumerateArray()
            .Select(item => item.GetString()!).OrderBy(text => text, StringComparer.Ordinal).ToArray();
        Assert.Equal(EnvelopeRequired, required);
    }

    /// <summary>Every sample payload satisfies the schema the server writes against.</summary>
    [Fact]
    public void EverySampleSatisfiesTheSchema()
    {
        foreach (Sample sample in Samples)
        {
            IReadOnlyList<string> errors = ValidateBody(sample.Json);
            Assert.True(errors.Count == 0, $"{sample.Kind}: {string.Join("; ", errors)}");
        }
    }

    /// <summary>Every sample reads into the model as the kind it says it is.</summary>
    [Fact]
    public void EverySampleReadsToItsKind()
    {
        foreach (Sample sample in Samples)
        {
            EffectReadResult read = EffectWire.Read(BatchJson(sample.Json));
            Assert.True(read.Applied, $"{sample.Kind}: the batch was refused");
            Assert.True(read.Problems.Count == 0, $"{sample.Kind}: {string.Join("; ", read.Problems)}");
            Effect effect = Assert.Single(read.Batch.Effects);
            Assert.Equal(sample.Kind, effect.Kind);
        }
    }

    /// <summary>The model writes payloads the schema accepts, and they read back unchanged.</summary>
    [Fact]
    public void TheModelWritesWhatTheSchemaAccepts()
    {
        foreach (Sample sample in Samples)
        {
            string written = EffectWire.Write(sample.Model);
            IReadOnlyList<string> errors = ValidateBody(written);
            Assert.True(errors.Count == 0, $"{sample.Kind}: the model wrote {written}: {string.Join("; ", errors)}");

            EffectReadResult read = EffectWire.Read(BatchJson(written));
            Assert.True(read.Problems.Count == 0, $"{sample.Kind}: {string.Join("; ", read.Problems)}");
            Assert.Equal(sample.Kind, Assert.Single(read.Batch.Effects).Kind);

            // The writer is canonical: reading and re-writing changes nothing.
            Assert.Equal(written, EffectWire.Write(Assert.Single(read.Batch.Effects)));
        }
    }

    /// <summary>A missing required member is a schema error and an unreadable effect.</summary>
    [Fact]
    public void AMissingRequiredMemberIsRefusedByTheSchemaAndTheReader()
    {
        foreach (Sample sample in Samples)
        {
            EffectKindShape shape = EffectSchema.For(sample.Kind)!;
            string member = shape.RequiredMemberNames.First(name => name != "kind");
            JsonObject body = JsonNode.Parse(sample.Json)!.AsObject();
            body.Remove(member);

            string mutated = body.ToJsonString();
            Assert.NotEmpty(ValidateBody(mutated));
            EffectReadResult read = EffectWire.Read(BatchJson(mutated));
            Assert.True(read.Batch.IsEmpty, $"{sample.Kind}: `{member}` is required");
            Assert.Contains(read.Problems, problem => problem.Code == EffectCodes.EffectInvalid);
        }
    }

    /// <summary>An unknown kind is a schema error and is skipped by the reader, not thrown.</summary>
    [Fact]
    public void AnUnknownKindIsRefusedByTheSchemaAndSkippedByTheReader()
    {
        foreach (Sample sample in Samples)
        {
            JsonObject body = JsonNode.Parse(sample.Json)!.AsObject();
            body["kind"] = "sparkle";
            string mutated = body.ToJsonString();
            Assert.NotEmpty(ValidateBody(mutated));

            EffectReadResult read = EffectWire.Read(BatchJson(mutated));
            Assert.True(read.Applied, "an unknown kind must not refuse the batch");
            Assert.True(read.Batch.IsEmpty);
            Assert.Contains(read.Problems, problem => problem.Code == EffectCodes.EffectUnknown);
        }
    }

    /// <summary>A member of the wrong type is refused in both directions.</summary>
    [Fact]
    public void AWrongTypeIsRefusedByBoth()
    {
        foreach (Sample sample in Samples)
        {
            EffectKindShape shape = EffectSchema.For(sample.Kind)!;
            EffectMemberShape? member = shape.Members.FirstOrDefault(candidate =>
                candidate.Name != "kind" && candidate.Type is "boolean" or "string" or "path" or "ref:effect_list");
            if (member is null)
            {
                continue;
            }

            JsonObject body = JsonNode.Parse(sample.Json)!.AsObject();
            body[member.Name] = 12;
            string mutated = body.ToJsonString();
            Assert.NotEmpty(ValidateBody(mutated));
            EffectReadResult read = EffectWire.Read(BatchJson(mutated));
            Assert.True(read.Batch.IsEmpty, $"{sample.Kind}.{member.Name} is not a number");
            Assert.NotEmpty(read.Problems);
        }
    }

    /// <summary>
    /// The deliberate asymmetry: a member v1 does not define is a schema error (the server
    /// must not write it) but the client ignores it, which is how a newer server stays
    /// compatible with an older client.
    /// </summary>
    [Fact]
    public void AnUnknownMemberIsRefusedByTheSchemaButIgnoredByTheReader()
    {
        foreach (Sample sample in Samples)
        {
            JsonObject body = JsonNode.Parse(sample.Json)!.AsObject();
            body["sparkle"] = true;
            string mutated = body.ToJsonString();
            Assert.NotEmpty(ValidateBody(mutated));

            EffectReadResult read = EffectWire.Read(BatchJson(mutated));
            Assert.True(read.Problems.Count == 0, $"{sample.Kind}: {string.Join("; ", read.Problems)}");
            Assert.Equal(sample.Kind, Assert.Single(read.Batch.Effects).Kind);
        }
    }

    /// <summary>A path with whitespace passes the reader's shape check and is refused when the
    /// applier resolves it — with a diagnostic, never an exception.</summary>
    [Fact]
    public void APathTheSchemaRefusesIsADiagnosticWhenApplied()
    {
        const string Body = """{"kind":"focus","path":"bad path"}""";
        Assert.NotEmpty(ValidateBody(Body));

        FormState form = TestForm.Load();
        EffectReadResult read = EffectWire.Read(BatchJson(Body));
        Assert.True(read.Problems.Count == 0, "the shape is fine; only the path is not");

        ApplyResult result = new EffectApplier().Apply(form, read.Batch);
        Assert.True(result.Applied);
        Assert.Contains(result.Problems, problem => problem.Code == EffectCodes.PathInvalid);
        Assert.Empty(result.ChangedPaths);
        Assert.Null(form.Focus);
    }

    private static IReadOnlyList<string> ValidateBody(string body)
    {
        using JsonDocument document = JsonDocument.Parse(body);
        return JsonSchemaLite.Validate(
            Schema.RootElement,
            Schema.RootElement.GetProperty("$defs").GetProperty("effect"),
            document.RootElement);
    }

    private static string BatchJson(string effectJson) =>
        string.Create(CultureInfo.InvariantCulture, $"{{\"version\":1,\"effects\":[{effectJson}]}}");

    private static string DeclaredType(JsonElement property, JsonElement defs)
    {
        if (property.TryGetProperty("$ref", out JsonElement reference))
        {
            return RefType(reference.GetString()!);
        }

        if (property.TryGetProperty("type", out JsonElement type))
        {
            string name = type.GetString() ?? throw new FormatException("a schema type is a string");
            if (name != "array")
            {
                return name;
            }

            JsonElement items = property.GetProperty("items");
            if (items.TryGetProperty("$ref", out JsonElement itemReference))
            {
                return RefType(itemReference.GetString()!) + "_list";
            }

            return "array";
        }

        if (property.TryGetProperty("const", out JsonElement constant) && constant.ValueKind == JsonValueKind.String)
        {
            // The `kind` member: a constant string, which is how the kind is stamped.
            return "string";
        }

        if (property.TryGetProperty("pattern", out _))
        {
            return "path";
        }

        return "value";
    }

    private static string RefType(string reference)
    {
        string name = reference.Split('/')[^1];
        return name switch
        {
            "path" => "path",
            "effect" => "ref:effect",
            _ => "ref:" + name,
        };
    }

    private static JsonDocument LoadSchema()
    {
        DirectoryInfo? directory = new(AppContext.BaseDirectory);
        while (directory is not null)
        {
            string candidate = Path.Combine(
                directory.FullName, "crates", "framework", "spot-form-effects", "schema", "effects.v1.json");
            if (File.Exists(candidate))
            {
                return JsonDocument.Parse(File.ReadAllText(candidate));
            }

            directory = directory.Parent;
        }

        throw new InvalidOperationException("the effects schema is not found relative to the test assembly");
    }

    private static List<Sample> BuildSamples()
    {
        var samples = new List<Sample>
        {
            new(
                "set_value",
                """{"kind":"set_value","path":"qty","value":5}""",
                new SetValueEffect("qty", "5")),
            new(
                "set_value",
                """{"kind":"set_value","path":"amount","value":"12.50"}""",
                new SetValueEffect("amount", "\"12.50\"")),
            new(
                "set_read_only",
                """{"kind":"set_read_only","path":"note","read_only":true}""",
                new SetReadOnlyEffect("note", true)),
            new(
                "set_hidden",
                """{"kind":"set_hidden","path":"note","hidden":false}""",
                new SetHiddenEffect("note", false)),
            new(
                "set_required",
                """{"kind":"set_required","path":"qty","required":true}""",
                new SetRequiredEffect("qty", true)),
            new(
                "set_options",
                """{"kind":"set_options","path":"status","options":[{"value":"open","label":"Open for work"},{"value":"draft"}]}""",
                new SetOptionsEffect("status", Array.AsReadOnly(new[]
                {
                    new OptionItem("open", "Open for work"),
                    new OptionItem("draft", null),
                }))),
            new(
                "set_label",
                """{"kind":"set_label","path":"qty","label":"Quantity"}""",
                new SetLabelEffect("qty", "Quantity")),
            new(
                "add_action",
                """{"kind":"add_action","id":"pick","label":"Pick","effect":{"kind":"focus","path":"qty"}}""",
                new AddActionEffect("pick", "Pick", new FocusEffect("qty"))),
            new(
                "remove_action",
                """{"kind":"remove_action","id":"pick"}""",
                new RemoveActionEffect("pick")),
            new(
                "show_message",
                """{"kind":"show_message","level":"warning","text":"Check it","field":"qty"}""",
                new ShowMessageEffect(MessageLevel.Warning, "Check it", null, "qty")),
            new(
                "show_message",
                """{"kind":"show_message","level":"error","text_key":"error.form_effects.script_failed"}""",
                new ShowMessageEffect(MessageLevel.Error, null, "error.form_effects.script_failed", null)),
            new(
                "open_dialog",
                """{"kind":"open_dialog","dialog_id":"pick_customer","params":{"search":"ac"}}""",
                new OpenDialogEffect("pick_customer", """{"search":"ac"}""")),
            new(
                "focus",
                """{"kind":"focus","path":"items[1].qty"}""",
                new FocusEffect("items[1].qty")),
            new(
                "set_filter",
                """{"kind":"set_filter","link_path":"customer","filter":"doc.status == 'open'"}""",
                new SetFilterEffect("customer", "doc.status == 'open'")),
            new(
                "batch",
                """{"kind":"batch","effects":[{"kind":"focus","path":"qty"}]}""",
                new BatchEffect(Array.AsReadOnly(new Effect[] { new FocusEffect("qty") }))),
        };

        // Every kind of v1 has a sample, or the schema could declare a kind the model never
        // reads (the structural test would catch the reverse).
        foreach (string kind in EffectsProtocol.AllKinds)
        {
            Assert.Contains(samples, sample => sample.Kind == kind);
        }

        return samples;
    }

    /// <summary>One sample payload: its kind, the JSON the server would write, and the model value.</summary>
    private sealed record Sample(string Kind, string Json, Effect Model);
}
