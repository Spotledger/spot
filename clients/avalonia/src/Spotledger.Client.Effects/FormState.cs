using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects;

/// <summary>One action on the form: a label and the effect choosing it applies.</summary>
/// <param name="Id">The action's id, unique on the form.</param>
/// <param name="Label">The label the client shows.</param>
/// <param name="OnChosen">What choosing the action does: data the applier applies, never code.</param>
public sealed record FormAction(string Id, string Label, Effect OnChosen);

/// <summary>One message the form shows, as state a renderer displays.</summary>
/// <param name="Level">How important it is.</param>
/// <param name="Text">The text, when the script supplied one.</param>
/// <param name="TextKey">The catalogue key, when the text is translated.</param>
/// <param name="Field">The field the message is about, when it is about one.</param>
public sealed record FormMessage(MessageLevel Level, string? Text, string? TextKey, string? Field);

/// <summary>One dialog the form asks to open, as an intent a renderer performs.</summary>
/// <param name="DialogId">The registered dialog id.</param>
/// <param name="ParamsJson">The dialog's parameters as JSON text, or null for none.</param>
public sealed record DialogIntent(string DialogId, string? ParamsJson);

/// <summary>The payload of a change a batch made to the form: the path and the effect kind.</summary>
/// <param name="Path">The field path the change is about.</param>
/// <param name="Kind">The effect kind that made it.</param>
public sealed class EffectChangeEventArgs : EventArgs
{
    /// <summary>Creates the event payload.</summary>
    public EffectChangeEventArgs(string path, string kind)
    {
        Path = path;
        Kind = kind;
    }

    /// <summary>The field path the change is about.</summary>
    public string Path { get; }

    /// <summary>The effect kind that made the change.</summary>
    public string Kind { get; }
}

/// <summary>
/// The form a client is showing, as the effects protocol sees it: the metadata runtime
/// core's <see cref="DocumentDraft"/> for values and dirty tracking, and the presentation
/// state, actions, messages and dialog intents a batch adds on top. It knows what the form
/// shows, not how it looks: a renderer subscribes to <see cref="ValueApplied"/> and
/// <see cref="PresentationChanged"/> to repaint and to re-evaluate its dependent rules.
/// </summary>
public sealed class FormState
{
    private readonly List<FormAction> _actions = new();
    private readonly List<FormMessage> _messages = new();
    private readonly List<DialogIntent> _dialogs = new();

    /// <summary>Creates the form state over a loaded draft.</summary>
    /// <param name="draft">The document draft whose values the form edits.</param>
    public FormState(DocumentDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        Draft = draft;
    }

    /// <summary>The document draft: values, dirty tracking and the merge patch.</summary>
    public DocumentDraft Draft { get; }

    /// <summary>The overrides the current batch set (read-only, hidden, required, options, labels, filters).</summary>
    public PresentationState Presentation { get; } = new();

    /// <summary>The actions the current batch left on the form, in display order.</summary>
    public IReadOnlyList<FormAction> Actions => _actions;

    /// <summary>The messages the current round raised, in order.</summary>
    public IReadOnlyList<FormMessage> Messages => _messages;

    /// <summary>The dialogs the current round asked to open, in order.</summary>
    public IReadOnlyList<DialogIntent> Dialogs => _dialogs;

    /// <summary>The path the current round moved the keyboard focus to, when it did.</summary>
    public string? Focus => Presentation.Focus;

    /// <summary>Raised after a value changed: the moment a renderer re-evaluates dependent rules.</summary>
    public event EventHandler<EffectChangeEventArgs>? ValueApplied;

    /// <summary>Raised after presentation state changed (a flag, options, a label, a filter, an action).</summary>
    public event EventHandler<EffectChangeEventArgs>? PresentationChanged;

    /// <summary>
    /// Whether the field at <paramref name="path"/> is read-only now: the batch's override,
    /// the DocType's own declaration, or the permission snapshot all count.
    /// </summary>
    public bool IsReadOnly(string path)
    {
        if (Presentation.IsReadOnly(path))
        {
            return true;
        }

        return TryResolve(path, out FieldTarget target, out _)
            && (target.Field.ReadOnly || target.State is { IsEditable: false } || target.State is { IsMasked: true });
    }

    /// <summary>Whether the batch hid the field at <paramref name="path"/>.</summary>
    public bool IsHidden(string path) => Presentation.IsHidden(path);

    /// <summary>Whether the field at <paramref name="path"/> is required now (its declaration or the batch).</summary>
    public bool IsRequired(string path)
    {
        if (Presentation.IsRequired(path))
        {
            return true;
        }

        return TryResolve(path, out FieldTarget target, out _) && target.Field.Required;
    }

    /// <summary>The paths the user (or a batch) changed since the draft was loaded, in field order.</summary>
    public IReadOnlyList<string> DirtyPaths() => Draft.DirtyPaths();

    /// <summary>
    /// Forgets the intents a previous round raised (messages, dialogs, the focus request):
    /// they belong to one round trip, while values and presentation overrides persist. A
    /// renderer calls this once it has displayed the round's intents.
    /// </summary>
    public void ClearIntents()
    {
        _messages.Clear();
        _dialogs.Clear();
        Presentation.ClearFocus();
    }

    /// <summary>The current value at <paramref name="path"/>, when the form has one.</summary>
    public FieldValue? ValueAt(string path) =>
        TryResolve(path, out FieldTarget target, out _) ? target.State?.Value : null;

    /// <summary>
    /// Writes <paramref name="value"/> at <paramref name="path"/> through the draft, so dirty
    /// tracking and the change notification are the draft's. Answers false when the form has
    /// no such field, column or row. A renderer restoring a local edit after a round uses
    /// this; the effect applier uses it once it has validated the value.
    /// </summary>
    public bool TrySetValue(string path, FieldValue value) =>
        TryResolve(path, out FieldTarget target, out _) && TrySetValue(target, value);

    /// <summary>Writes a resolved target's value; see <see cref="TrySetValue(string, FieldValue)"/>.</summary>
    internal bool TrySetValue(FieldTarget target, FieldValue value)
    {
        ArgumentNullException.ThrowIfNull(target);
        if (target.IsRow)
        {
            DocumentDraft.TableDraft.RowDraft row = target.Table!.Rows[target.RowIndex];
            row.SetValue(target.Field.Name, value ?? FieldValue.Null);
            if (row.Values.TryGetValue(target.Field.Name, out FieldState? cell))
            {
                cell.ClearErrors();
            }
        }
        else
        {
            Draft.Set(target.Field.Name, value ?? FieldValue.Null);
            if (Draft.Fields.TryGetValue(target.Field.Name, out FieldState? state))
            {
                state.ClearErrors();
            }
        }

        NoteValueApplied(target.Wire);
        return true;
    }

    /// <summary>
    /// Resolves a wire path to its field model and state: a top-level field, or a row column
    /// of a child table. Answers false with the problem to report when the form has no such
    /// field, table, column or row.
    /// </summary>
    internal bool TryResolve(string text, out FieldTarget target, out EffectProblem? problem) =>
        FormResolution.TryResolve(this, text, out target, out problem);

    internal void AddMessage(FormMessage message) => _messages.Add(message);

    internal void RequestDialog(DialogIntent dialog) => _dialogs.Add(dialog);

    internal void SetFocus(string path)
    {
        Presentation.Focus = path;
        PresentationChanged?.Invoke(this, new EffectChangeEventArgs(path, "focus"));
    }

    internal void AddOrReplaceAction(FormAction action)
    {
        int found = _actions.FindIndex(existing => string.Equals(existing.Id, action.Id, StringComparison.Ordinal));
        if (found >= 0)
        {
            _actions[found] = action;
        }
        else
        {
            _actions.Add(action);
        }

        PresentationChanged?.Invoke(this, new EffectChangeEventArgs(action.Id, "add_action"));
    }

    internal void RemoveAction(string id)
    {
        if (_actions.RemoveAll(existing => string.Equals(existing.Id, id, StringComparison.Ordinal)) > 0)
        {
            PresentationChanged?.Invoke(this, new EffectChangeEventArgs(id, "remove_action"));
        }
    }

    internal void NoteValueApplied(string path)
    {
        ValueApplied?.Invoke(this, new EffectChangeEventArgs(path, "set_value"));
    }

    internal void NotePresentationChanged(string path, string kind) =>
        PresentationChanged?.Invoke(this, new EffectChangeEventArgs(path, kind));
}

/// <summary>One resolved effect target: its field model, its state and, for a row, its place.</summary>
/// <param name="Field">The top-level field's model, or the row column's model.</param>
/// <param name="State">The field state holding the value, when it exists.</param>
/// <param name="Table">The table draft, for a row path.</param>
/// <param name="RowIndex">The row's 0-based position, for a row path.</param>
/// <param name="Wire">The canonical wire path (the 1-based index for a row).</param>
internal sealed record FieldTarget(
    FieldModel Field,
    FieldState? State,
    DocumentDraft.TableDraft? Table,
    int RowIndex,
    string Wire)
{
    /// <summary>Whether the target is a row column rather than a top-level field.</summary>
    public bool IsRow => Table is not null;
}

/// <summary>The one place a wire path is resolved against a form state.</summary>
internal static class FormResolution
{
    /// <summary>Resolves <paramref name="text"/> (see <see cref="FormState.TryResolve"/>).</summary>
    public static bool TryResolve(FormState state, string text, out FieldTarget target, out EffectProblem? problem)
    {
        ArgumentNullException.ThrowIfNull(state);
        target = null!;
        problem = null;

        if (!EffectPath.TryParse(text, out EffectPath path))
        {
            problem = EffectProblem.Of(
                EffectCodes.PathInvalid,
                "An effect path must be `field` or `table[idx].field` with idx from 1.");
            return false;
        }

        if (!path.IsRow)
        {
            FieldModel? field = state.Draft.Model.Field(path.Field);
            if (field is null)
            {
                problem = Unknown(text);
                return false;
            }

            state.Draft.Fields.TryGetValue(path.Field, out FieldState? fieldState);
            target = new FieldTarget(field, fieldState, null, 0, path.Wire());
            return true;
        }

        FieldModel? tableField = state.Draft.Model.Field(path.Table!);
        if (tableField is null || tableField.Kind != FieldKind.Table)
        {
            problem = Unknown(text);
            return false;
        }

        if (!state.Draft.Tables.TryGetValue(path.Table!, out DocumentDraft.TableDraft? table))
        {
            problem = Unknown(text);
            return false;
        }

        DocTypeModel? rowModel = table.RowModel;
        FieldModel? column = rowModel?.Field(path.Field);
        int index = path.Index!.Value - 1;
        if (column is null || index < 0 || index >= table.Rows.Count)
        {
            problem = Unknown(text);
            return false;
        }

        table.Rows[index].Values.TryGetValue(path.Field, out FieldState? cellState);
        target = new FieldTarget(column, cellState, table, index, path.Wire());
        return true;
    }

    /// <summary>The problem for a field, table, column or row the form does not have.</summary>
    public static EffectProblem Unknown(string text) =>
        EffectProblem.Of(EffectCodes.PathUnknown, $"The form has no field at `{text}`.").WithPath(text);
}
