namespace Spotledger.Client.Effects;

/// <summary>
/// The <c>form_effects.*</c> codes of the server's error catalogue
/// (<c>crates/framework/spot-form-effects/src/codes.rs</c>), restated here so a client
/// diagnostic carries the code the server would have used for the same problem.
/// </summary>
public static class EffectCodes
{
    /// <summary>A batch names a protocol version this client does not speak.</summary>
    public const string ProtocolVersion = "form_effects.protocol_version";

    /// <summary>An effect's <c>kind</c> is not one this protocol version knows (skipped).</summary>
    public const string EffectUnknown = "form_effects.effect_unknown";

    /// <summary>An effect does not have the object shape its kind requires.</summary>
    public const string EffectInvalid = "form_effects.effect_invalid";

    /// <summary>A field path is not <c>field</c> or <c>table[idx].field</c>.</summary>
    public const string PathInvalid = "form_effects.path_invalid";

    /// <summary>The DocType has no such field, table, column or row.</summary>
    public const string PathUnknown = "form_effects.path_unknown";

    /// <summary>The field exists but this user may not see it; the problem never names it.</summary>
    public const string PathUnavailable = "form_effects.path_unavailable";

    /// <summary>The value does not fit the field's kind, scale, length or listed values.</summary>
    public const string ValueInvalid = "form_effects.value_invalid";

    /// <summary>This user may not write the field, or the field is read-only.</summary>
    public const string FieldNotWritable = "form_effects.field_not_writable";

    /// <summary>A select or link's candidate values are malformed, duplicated or too many.</summary>
    public const string OptionInvalid = "form_effects.option_invalid";

    /// <summary>A label is empty or too long.</summary>
    public const string LabelInvalid = "form_effects.label_invalid";

    /// <summary>A message has neither text nor a catalogue key, or its text is too long.</summary>
    public const string MessageInvalid = "form_effects.message_invalid";

    /// <summary>An action's id or label is malformed.</summary>
    public const string ActionInvalid = "form_effects.action_invalid";

    /// <summary>A dialog is not registered for this form.</summary>
    public const string DialogUnknown = "form_effects.dialog_unknown";

    /// <summary>A filter expression does not parse, or the field it is set on is not a link.</summary>
    public const string FilterInvalid = "form_effects.filter_invalid";

    /// <summary>A batch produced more effects, or larger ones, than the caps allow.</summary>
    public const string LimitExceeded = "form_effects.limit_exceeded";
}
