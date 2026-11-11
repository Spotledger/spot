using System.Text;
using System.Text.Json;

namespace Spotledger.Client.Effects;

/// <summary>The outcome of reading a batch off the wire.</summary>
/// <param name="Batch">The effects a client may apply; empty when the batch was refused whole.</param>
/// <param name="Problems">Every problem met on the way, in the order they were met.</param>
/// <param name="Applied">False when the batch was refused whole (an unknown protocol
/// version or a body that is not a batch at all), so nothing may be applied.</param>
public sealed record EffectReadResult(EffectBatch Batch, IReadOnlyList<EffectProblem> Problems, bool Applied);

/// <summary>
/// Reads and writes the effects protocol's JSON. Reading never throws: an unknown kind is
/// skipped with <c>form_effects.effect_unknown</c>, an effect that does not have its kind's
/// shape is dropped with <c>form_effects.effect_invalid</c>, unknown object members are
/// ignored, and a version this client does not speak refuses the batch whole with
/// <c>form_effects.protocol_version</c>.
/// </summary>
public static class EffectWire
{
    private const int MaxReadDepth = 32;

    private static readonly JsonDocumentOptions DocumentOptions = new()
    {
        AllowTrailingCommas = false,
        CommentHandling = JsonCommentHandling.Disallow,
    };

    /// <summary>Reads a batch body (<c>{"version":1,"effects":[…]}</c>).</summary>
    public static EffectReadResult Read(string json)
    {
        if (json is null)
        {
            return Refused(EffectProblem.Of(EffectCodes.EffectInvalid, "A batch body is JSON text."));
        }

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(json, DocumentOptions);
        }
        catch (JsonException)
        {
            return Refused(EffectProblem.Of(EffectCodes.EffectInvalid, "A batch body is valid JSON."));
        }

        using (document)
        {
            return ReadRoot(document.RootElement);
        }
    }

    /// <summary>Reads one effect from a JSON element (a batch's item, or a nested effect).</summary>
    public static bool TryReadEffect(JsonElement element, out Effect? effect, out EffectProblem? problem) =>
        TryReadOne(element, 0, out effect, out problem);

    /// <summary>Writes a batch in the protocol's canonical member order.</summary>
    public static string Write(EffectBatch batch)
    {
        ArgumentNullException.ThrowIfNull(batch);
        return WriteJson(writer =>
        {
            writer.WriteStartObject();
            writer.WriteNumber("version", batch.Version);
            writer.WritePropertyName("effects");
            WriteEffects(writer, batch.Effects);
            writer.WriteEndObject();
        });
    }

    /// <summary>Writes one effect in the protocol's canonical member order.</summary>
    public static string Write(Effect effect)
    {
        ArgumentNullException.ThrowIfNull(effect);
        return WriteJson(writer => WriteEffect(writer, effect));
    }

    private static EffectReadResult ReadRoot(JsonElement root)
    {
        if (root.ValueKind != JsonValueKind.Object)
        {
            return Refused(EffectProblem.Of(EffectCodes.EffectInvalid, "A batch is an object."));
        }

        if (!root.TryGetProperty("version", out JsonElement versionElement)
            || versionElement.ValueKind != JsonValueKind.Number
            || !versionElement.TryGetInt32(out int version))
        {
            return Refused(EffectProblem.Of(
                EffectCodes.ProtocolVersion, "An effects batch names its protocol version."));
        }

        if (version != EffectsProtocol.Version)
        {
            return Refused(EffectProblem.Of(
                EffectCodes.ProtocolVersion,
                $"Protocol version {version} is not this client's {EffectsProtocol.Version}."));
        }

        if (!root.TryGetProperty("effects", out JsonElement list) || list.ValueKind != JsonValueKind.Array)
        {
            return Refused(EffectProblem.Of(
                EffectCodes.EffectInvalid, "An effects batch carries an `effects` array."));
        }

        var effects = new List<Effect>(list.GetArrayLength());
        var problems = new List<EffectProblem>();
        int index = 0;
        foreach (JsonElement item in list.EnumerateArray())
        {
            if (TryReadOne(item, 0, out Effect? effect, out EffectProblem? problem) && effect is not null)
            {
                effects.Add(effect);
            }
            else if (problem is not null)
            {
                problems.Add(problem.WithIndex(index));
            }

            index++;
        }

        return new EffectReadResult(EffectBatch.Of(effects), problems, true);
    }

    private static bool TryReadOne(JsonElement element, int depth, out Effect? effect, out EffectProblem? problem)
    {
        effect = null;
        problem = null;
        if (depth > MaxReadDepth)
        {
            problem = EffectProblem.Of(EffectCodes.EffectInvalid, "An effect is nested too deeply.");
            return false;
        }

        if (element.ValueKind != JsonValueKind.Object)
        {
            problem = EffectProblem.Of(EffectCodes.EffectInvalid, "An effect is an object with a `kind`.");
            return false;
        }

        if (!element.TryGetProperty("kind", out JsonElement kindElement)
            || kindElement.ValueKind != JsonValueKind.String)
        {
            problem = EffectProblem.Of(EffectCodes.EffectInvalid, "An effect is an object with a `kind`.");
            return false;
        }

        string kind = kindElement.GetString() ?? string.Empty;
        EffectKindShape? shape = EffectSchema.For(kind);
        if (shape is null)
        {
            problem = EffectProblem.Of(
                EffectCodes.EffectUnknown,
                $"`{kind}` is not an effect this protocol version knows.");
            return false;
        }

        if (!TryBuild(element, kind, depth, out effect) || effect is null)
        {
            effect = null;
            problem = EffectProblem.Of(
                EffectCodes.EffectInvalid,
                $"The effect does not have the shape `{shape.Kind}` requires.");
            return false;
        }

        return true;
    }

    private static bool TryBuild(JsonElement element, string kind, int depth, out Effect? effect)
    {
        effect = null;
        switch (kind)
        {
            case "set_value":
                if (!TryText(element, "path", out string? valuePath) || !element.TryGetProperty("value", out JsonElement value))
                {
                    return false;
                }

                effect = new SetValueEffect(valuePath!, value.GetRawText());
                return true;

            case "set_read_only":
                if (!TryText(element, "path", out string? readOnlyPath) || !TryFlag(element, "read_only", out bool readOnly))
                {
                    return false;
                }

                effect = new SetReadOnlyEffect(readOnlyPath!, readOnly);
                return true;

            case "set_hidden":
                if (!TryText(element, "path", out string? hiddenPath) || !TryFlag(element, "hidden", out bool hidden))
                {
                    return false;
                }

                effect = new SetHiddenEffect(hiddenPath!, hidden);
                return true;

            case "set_required":
                if (!TryText(element, "path", out string? requiredPath) || !TryFlag(element, "required", out bool required))
                {
                    return false;
                }

                effect = new SetRequiredEffect(requiredPath!, required);
                return true;

            case "set_options":
                if (!TryText(element, "path", out string? optionsPath)
                    || !element.TryGetProperty("options", out JsonElement options)
                    || options.ValueKind != JsonValueKind.Array)
                {
                    return false;
                }

                var items = new List<OptionItem>(options.GetArrayLength());
                foreach (JsonElement option in options.EnumerateArray())
                {
                    if (option.ValueKind != JsonValueKind.Object
                        || !option.TryGetProperty("value", out JsonElement optionValue)
                        || optionValue.ValueKind != JsonValueKind.String)
                    {
                        return false;
                    }

                    string? optionLabel = null;
                    if (option.TryGetProperty("label", out JsonElement labelElement))
                    {
                        if (labelElement.ValueKind != JsonValueKind.String)
                        {
                            return false;
                        }

                        optionLabel = labelElement.GetString();
                    }

                    items.Add(new OptionItem(optionValue.GetString() ?? string.Empty, optionLabel));
                }

                effect = new SetOptionsEffect(optionsPath!, Array.AsReadOnly(items.ToArray()));
                return true;

            case "set_label":
                if (!TryText(element, "path", out string? labelPath) || !TryText(element, "label", out string? label))
                {
                    return false;
                }

                effect = new SetLabelEffect(labelPath!, label!);
                return true;

            case "add_action":
                if (!TryText(element, "id", out string? actionId)
                    || !TryText(element, "label", out string? actionLabel)
                    || !element.TryGetProperty("effect", out JsonElement inner)
                    || !TryReadOne(inner, depth + 1, out Effect? innerEffect, out _)
                    || innerEffect is null)
                {
                    return false;
                }

                effect = new AddActionEffect(actionId!, actionLabel!, innerEffect);
                return true;

            case "remove_action":
                if (!TryText(element, "id", out string? removeId))
                {
                    return false;
                }

                effect = new RemoveActionEffect(removeId!);
                return true;

            case "show_message":
                if (!element.TryGetProperty("level", out JsonElement levelElement)
                    || levelElement.ValueKind != JsonValueKind.String
                    || !MessageLevelNames.TryParse(levelElement.GetString(), out MessageLevel level))
                {
                    return false;
                }

                string? messageText = null;
                if (element.TryGetProperty("text", out JsonElement textElement))
                {
                    if (textElement.ValueKind != JsonValueKind.String)
                    {
                        return false;
                    }

                    messageText = textElement.GetString();
                }

                string? textKey = null;
                if (element.TryGetProperty("text_key", out JsonElement keyElement))
                {
                    if (keyElement.ValueKind != JsonValueKind.String)
                    {
                        return false;
                    }

                    textKey = keyElement.GetString();
                }

                string? field = null;
                if (element.TryGetProperty("field", out JsonElement fieldElement))
                {
                    if (fieldElement.ValueKind != JsonValueKind.String)
                    {
                        return false;
                    }

                    field = fieldElement.GetString();
                }

                if (string.IsNullOrEmpty(messageText) && string.IsNullOrEmpty(textKey))
                {
                    return false;
                }

                effect = new ShowMessageEffect(level, messageText, textKey, field);
                return true;

            case "open_dialog":
                if (!TryText(element, "dialog_id", out string? dialogId))
                {
                    return false;
                }

                string? parameters = null;
                if (element.TryGetProperty("params", out JsonElement paramsElement)
                    && paramsElement.ValueKind != JsonValueKind.Null)
                {
                    if (paramsElement.ValueKind != JsonValueKind.Object)
                    {
                        return false;
                    }

                    parameters = paramsElement.GetRawText();
                }

                effect = new OpenDialogEffect(dialogId!, parameters);
                return true;

            case "focus":
                if (!TryText(element, "path", out string? focusPath))
                {
                    return false;
                }

                effect = new FocusEffect(focusPath!);
                return true;

            case "set_filter":
                if (!TryText(element, "link_path", out string? linkPath) || !TryText(element, "filter", out string? filter))
                {
                    return false;
                }

                effect = new SetFilterEffect(linkPath!, filter!);
                return true;

            case "batch":
                if (!element.TryGetProperty("effects", out JsonElement nested)
                    || nested.ValueKind != JsonValueKind.Array)
                {
                    return false;
                }

                var effects = new List<Effect>(nested.GetArrayLength());
                foreach (JsonElement item in nested.EnumerateArray())
                {
                    if (!TryReadOne(item, depth + 1, out Effect? child, out _) || child is null)
                    {
                        return false;
                    }

                    effects.Add(child);
                }

                effect = new BatchEffect(Array.AsReadOnly(effects.ToArray()));
                return true;

            default:
                return false;
        }
    }

    private static bool TryText(JsonElement element, string name, out string? text)
    {
        text = null;
        if (!element.TryGetProperty(name, out JsonElement value) || value.ValueKind != JsonValueKind.String)
        {
            return false;
        }

        text = value.GetString();
        return !string.IsNullOrEmpty(text);
    }

    private static bool TryFlag(JsonElement element, string name, out bool flag)
    {
        flag = false;
        if (!element.TryGetProperty(name, out JsonElement value))
        {
            return false;
        }

        switch (value.ValueKind)
        {
            case JsonValueKind.True:
                flag = true;
                return true;
            case JsonValueKind.False:
                return true;
            default:
                return false;
        }
    }

    private static EffectReadResult Refused(EffectProblem problem) =>
        new(EffectBatch.Empty, new[] { problem }, false);

    private static string WriteJson(Action<Utf8JsonWriter> write)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            write(writer);
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    private static void WriteEffects(Utf8JsonWriter writer, IReadOnlyList<Effect> effects)
    {
        writer.WriteStartArray();
        foreach (Effect effect in effects)
        {
            WriteEffect(writer, effect);
        }

        writer.WriteEndArray();
    }

    private static void WriteEffect(Utf8JsonWriter writer, Effect effect)
    {
        ArgumentNullException.ThrowIfNull(effect);
        writer.WriteStartObject();
        writer.WriteString("kind", effect.Kind);
        switch (effect)
        {
            case SetValueEffect setValue:
                writer.WriteString("path", setValue.Path);
                writer.WritePropertyName("value");
                WriteRaw(writer, setValue.ValueJson);
                break;
            case SetReadOnlyEffect setReadOnly:
                writer.WriteString("path", setReadOnly.Path);
                writer.WriteBoolean("read_only", setReadOnly.ReadOnly);
                break;
            case SetHiddenEffect setHidden:
                writer.WriteString("path", setHidden.Path);
                writer.WriteBoolean("hidden", setHidden.Hidden);
                break;
            case SetRequiredEffect setRequired:
                writer.WriteString("path", setRequired.Path);
                writer.WriteBoolean("required", setRequired.Required);
                break;
            case SetOptionsEffect setOptions:
                writer.WriteString("path", setOptions.Path);
                writer.WritePropertyName("options");
                writer.WriteStartArray();
                foreach (OptionItem option in setOptions.Options)
                {
                    writer.WriteStartObject();
                    writer.WriteString("value", option.Value);
                    if (option.Label is { } label)
                    {
                        writer.WriteString("label", label);
                    }

                    writer.WriteEndObject();
                }

                writer.WriteEndArray();
                break;
            case SetLabelEffect setLabel:
                writer.WriteString("path", setLabel.Path);
                writer.WriteString("label", setLabel.Label);
                break;
            case AddActionEffect addAction:
                writer.WriteString("id", addAction.Id);
                writer.WriteString("label", addAction.Label);
                writer.WritePropertyName("effect");
                WriteEffect(writer, addAction.Inner);
                break;
            case RemoveActionEffect removeAction:
                writer.WriteString("id", removeAction.Id);
                break;
            case ShowMessageEffect showMessage:
                writer.WriteString("level", showMessage.Level.ToWireName());
                if (showMessage.Text is { } text)
                {
                    writer.WriteString("text", text);
                }

                if (showMessage.TextKey is { } textKey)
                {
                    writer.WriteString("text_key", textKey);
                }

                if (showMessage.Field is { } field)
                {
                    writer.WriteString("field", field);
                }

                break;
            case OpenDialogEffect openDialog:
                writer.WriteString("dialog_id", openDialog.DialogId);
                if (openDialog.ParamsJson is { } parameters)
                {
                    writer.WritePropertyName("params");
                    WriteRaw(writer, parameters);
                }

                break;
            case FocusEffect focus:
                writer.WriteString("path", focus.Path);
                break;
            case SetFilterEffect setFilter:
                writer.WriteString("link_path", setFilter.LinkPath);
                writer.WriteString("filter", setFilter.Filter);
                break;
            case BatchEffect batch:
                writer.WritePropertyName("effects");
                WriteEffects(writer, batch.Effects);
                break;
            default:
                break;
        }

        writer.WriteEndObject();
    }

    private static void WriteRaw(Utf8JsonWriter writer, string raw)
    {
        using JsonDocument document = JsonDocument.Parse(raw, DocumentOptions);
        document.RootElement.WriteTo(writer);
    }
}
