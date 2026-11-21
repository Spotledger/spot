using System.Globalization;

namespace Spotledger.Client.Effects;

/// <summary>
/// The UI effects protocol, v1: the closed set of declarative effects a server-side form
/// script may ask a client to apply.
/// </summary>
/// <remarks>
/// <para>
/// The client never runs app code (ADR-28). Imperative form logic runs on the server as a
/// TypeScript form script; what comes back is this protocol: plain data the client applies
/// in order. The same bytes are applied by the desktop and the browser head, and the
/// shared conformance corpus (<c>crates/framework/spot-form-effects/conformance/forms</c>)
/// is the proof.
/// </para>
/// <para>
/// Within v1 the protocol is additive only: a client skips an effect <em>kind</em> it does
/// not know and keeps applying the rest, ignores unknown object members, and refuses a
/// batch whose <c>version</c> it does not speak <em>whole</em> — applying part of a
/// protocol one does not understand is worse than applying none of it.
/// </para>
/// </remarks>
public static class EffectsProtocol
{
    /// <summary>The protocol version this client speaks.</summary>
    public const int Version = 1;

    /// <summary>The largest nesting a <c>batch</c> effect may reach, counting the batch itself.</summary>
    public const int MaxBatchDepth = 8;

    /// <summary>Every kind name of v1, in the order the specification lists them.</summary>
    public static IReadOnlyList<string> AllKinds { get; } = Array.AsReadOnly(new[]
    {
        "set_value",
        "set_read_only",
        "set_hidden",
        "set_required",
        "set_options",
        "set_label",
        "add_action",
        "remove_action",
        "show_message",
        "open_dialog",
        "focus",
        "set_filter",
        "batch",
    });
}

/// <summary>How important a message is.</summary>
public enum MessageLevel
{
    /// <summary>For information.</summary>
    Info,

    /// <summary>Something the user should check.</summary>
    Warning,

    /// <summary>A validation problem; the form should not be saved as it is.</summary>
    Error,
}

/// <summary>The wire name of a <see cref="MessageLevel"/> and its reverse.</summary>
public static class MessageLevelNames
{
    /// <summary>The wire name (<c>info</c>, <c>warning</c>, <c>error</c>).</summary>
    public static string ToWireName(this MessageLevel level) => level switch
    {
        MessageLevel.Info => "info",
        MessageLevel.Warning => "warning",
        MessageLevel.Error => "error",
        _ => "info",
    };

    /// <summary>Reads a level from its wire name; false when the name is unknown.</summary>
    public static bool TryParse(string? name, out MessageLevel level)
    {
        switch (name)
        {
            case "info":
                level = MessageLevel.Info;
                return true;
            case "warning":
                level = MessageLevel.Warning;
                return true;
            case "error":
                level = MessageLevel.Error;
                return true;
            default:
                level = MessageLevel.Info;
                return false;
        }
    }
}

/// <summary>One candidate value of a select or link field; the label defaults to the value.</summary>
/// <param name="Value">The stored value.</param>
/// <param name="Label">The text the client shows, when it differs from the value.</param>
public sealed record OptionItem(string Value, string? Label)
{
    /// <summary>The text a renderer shows for this candidate.</summary>
    public string DisplayLabel => Label ?? Value;
}

/// <summary>One effect a client applies to the form it is showing.</summary>
public abstract record Effect
{
    /// <summary>The effect's kind, as the wire names it.</summary>
    public abstract string Kind { get; }

    /// <summary>The field path the effect targets, when it targets one directly.</summary>
    public virtual string? TargetPath => null;
}

/// <summary>Put a value in a field, as if the user typed it.</summary>
/// <param name="Path">The target field path.</param>
/// <param name="ValueJson">The value exactly as the wire carried it (a decimal is its exact
/// string, never a float).</param>
public sealed record SetValueEffect(string Path, string ValueJson) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "set_value";

    /// <inheritdoc/>
    public override string? TargetPath => Path;
}

/// <summary>Make a field read-only, or editable again.</summary>
/// <param name="Path">The target field path.</param>
/// <param name="ReadOnly">True makes it read-only.</param>
public sealed record SetReadOnlyEffect(string Path, bool ReadOnly) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "set_read_only";

    /// <inheritdoc/>
    public override string? TargetPath => Path;
}

/// <summary>Hide a field, or show it again.</summary>
/// <param name="Path">The target field path.</param>
/// <param name="Hidden">True hides it.</param>
public sealed record SetHiddenEffect(string Path, bool Hidden) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "set_hidden";

    /// <inheritdoc/>
    public override string? TargetPath => Path;
}

/// <summary>Mark a field required, or not.</summary>
/// <param name="Path">The target field path.</param>
/// <param name="Required">True marks it required.</param>
public sealed record SetRequiredEffect(string Path, bool Required) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "set_required";

    /// <inheritdoc/>
    public override string? TargetPath => Path;
}

/// <summary>Replace the candidate values of a select or link field.</summary>
/// <param name="Path">The target field path.</param>
/// <param name="Options">The candidates, in display order.</param>
public sealed record SetOptionsEffect(string Path, IReadOnlyList<OptionItem> Options) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "set_options";

    /// <inheritdoc/>
    public override string? TargetPath => Path;
}

/// <summary>Replace a field's label.</summary>
/// <param name="Path">The target field path.</param>
/// <param name="Label">The new label.</param>
public sealed record SetLabelEffect(string Path, string Label) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "set_label";

    /// <inheritdoc/>
    public override string? TargetPath => Path;
}

/// <summary>Add a form action, with the effect it applies when chosen.</summary>
/// <param name="Id">The action's id, unique on the form.</param>
/// <param name="Label">The label the client shows.</param>
/// <param name="Inner">What choosing the action does: data, never code.</param>
public sealed record AddActionEffect(string Id, string Label, Effect Inner) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "add_action";
}

/// <summary>Remove an action by id.</summary>
/// <param name="Id">The action's id.</param>
public sealed record RemoveActionEffect(string Id) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "remove_action";
}

/// <summary>Show an info, warning or error message.</summary>
/// <param name="Level">How important the message is.</param>
/// <param name="Text">The text, when the script supplies one.</param>
/// <param name="TextKey">The catalogue key, when the text is translated.</param>
/// <param name="Field">The field the message is about, when it is about one.</param>
public sealed record ShowMessageEffect(MessageLevel Level, string? Text, string? TextKey, string? Field) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "show_message";

    /// <inheritdoc/>
    public override string? TargetPath => Field;
}

/// <summary>Ask the client to open a registered dialog.</summary>
/// <param name="DialogId">The dialog's registered id.</param>
/// <param name="ParamsJson">The dialog's parameters as JSON text, or null for none.</param>
public sealed record OpenDialogEffect(string DialogId, string? ParamsJson) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "open_dialog";
}

/// <summary>Move the keyboard focus to a field.</summary>
/// <param name="Path">The target field path.</param>
public sealed record FocusEffect(string Path) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "focus";

    /// <inheritdoc/>
    public override string? TargetPath => Path;
}

/// <summary>Set a link field's filter: a shared-expression-language expression over the document.</summary>
/// <param name="LinkPath">The link field whose candidate list is filtered.</param>
/// <param name="Filter">The filter expression.</param>
public sealed record SetFilterEffect(string LinkPath, string Filter) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "set_filter";

    /// <inheritdoc/>
    public override string? TargetPath => LinkPath;
}

/// <summary>Apply the nested effects in order (a transport convenience; the server flattens it).</summary>
/// <param name="Effects">The nested effects.</param>
public sealed record BatchEffect(IReadOnlyList<Effect> Effects) : Effect
{
    /// <inheritdoc/>
    public override string Kind => "batch";
}

/// <summary>The batch one form event answers: the effects a client applies, in order.</summary>
/// <param name="Version">The protocol version the batch is written in.</param>
/// <param name="Effects">The effects, in application order.</param>
public sealed record EffectBatch(int Version, IReadOnlyList<Effect> Effects)
{
    /// <summary>An empty v1 batch.</summary>
    public static EffectBatch Empty { get; } = new(EffectsProtocol.Version, Array.Empty<Effect>());

    /// <summary>A v1 batch of <paramref name="effects"/>.</summary>
    public static EffectBatch Of(params Effect[] effects) =>
        new(EffectsProtocol.Version, Array.AsReadOnly(effects));

    /// <summary>A v1 batch of <paramref name="effects"/>.</summary>
    public static EffectBatch Of(IReadOnlyList<Effect> effects) =>
        new(EffectsProtocol.Version, effects);

    /// <summary>Whether the batch has no effects.</summary>
    public bool IsEmpty => Effects.Count == 0;
}

/// <summary>
/// One problem met while reading or applying a batch: the code the server would use, the
/// effect's index in the batch when it has one, the field path when the problem is about
/// one, and a sentence safe to show or log.
/// </summary>
/// <param name="Code">The stable <c>form_effects.*</c> code.</param>
/// <param name="Index">The effect's position in the batch, when known.</param>
/// <param name="Path">The field path the problem names, when naming one is safe.</param>
/// <param name="Message">What is wrong, in a sentence.</param>
public sealed record EffectProblem(string Code, int? Index, string? Path, string Message)
{
    /// <summary>Creates a problem not tied to an effect's position.</summary>
    public static EffectProblem Of(string code, string message) => new(code, null, null, message);

    /// <summary>Creates a problem about one effect position.</summary>
    public static EffectProblem At(int index, string code, string message) => new(code, index, null, message);

    /// <summary>The same problem, with the effect's position filled in.</summary>
    public EffectProblem WithIndex(int index) => this with { Index = index };

    /// <summary>The same problem, naming the field path it is about.</summary>
    public EffectProblem WithPath(string? path) => this with { Path = path };

    /// <inheritdoc/>
    public override string ToString() =>
        string.Create(
            CultureInfo.InvariantCulture,
            $"{Code}{(Index is { } index ? $" at {index}" : string.Empty)}{(Path is { } path ? $" ({path})" : string.Empty)}: {Message}");
}
