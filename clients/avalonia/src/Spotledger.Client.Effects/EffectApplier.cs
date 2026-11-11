using System.Globalization;
using System.Text;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects;

/// <summary>
/// Applies an effects batch to a <see cref="FormState"/> over the metadata runtime core: the
/// client half of the protocol.
/// </summary>
/// <remarks>
/// <para>
/// Effects apply in the order the batch carries. A <c>batch</c> effect is flattened in place,
/// so nesting never changes the result. Every effect is resolved against the form before it is
/// applied: an unknown path, a value that does not fit the field, an unwritable field or a
/// malformed action is <em>dropped</em> with a diagnostic and never throws — one bad effect
/// must not cost the form the rest of the round. Order and the last-writer-wins rule are the
/// server's contract, so the client does not re-sort or de-duplicate.
/// </para>
/// <para>
/// A value effect goes through <see cref="DocumentDraft"/>'s typed setter, so dirty tracking
/// stays the draft's, and the form raises <see cref="FormState.ValueApplied"/> so a renderer
/// re-evaluates the rules that depend on the field. Nothing here reads a clock, rounds a
/// decimal or touches a renderer.
/// </para>
/// </remarks>
public sealed class EffectApplier
{
    /// <summary>Creates the applier.</summary>
    /// <param name="limits">The caps a batch is bounded by; the server's defaults when null.</param>
    /// <param name="registeredDialogs">The dialogs this client head registers, when it knows
    /// them; an effect naming another dialog is then dropped. Null accepts any well-formed
    /// dialog id, which is what a head that resolves dialogs lazily wants.</param>
    public EffectApplier(EffectLimits? limits = null, IReadOnlySet<string>? registeredDialogs = null)
    {
        Limits = limits ?? EffectLimits.Default;
        RegisteredDialogs = registeredDialogs;
    }

    /// <summary>The caps a batch is bounded by.</summary>
    public EffectLimits Limits { get; }

    /// <summary>The dialogs this head registers, when it knows them.</summary>
    public IReadOnlySet<string>? RegisteredDialogs { get; }

    /// <summary>
    /// Applies <paramref name="batch"/> to <paramref name="form"/>. Never throws for anything
    /// the wire can carry; the caller inspects <see cref="ApplyResult.Problems"/>.
    /// </summary>
    public ApplyResult Apply(FormState form, EffectBatch batch)
    {
        ArgumentNullException.ThrowIfNull(form);
        ArgumentNullException.ThrowIfNull(batch);
        var pass = new ApplyResult.Builder();
        if (batch.Version != EffectsProtocol.Version)
        {
            pass.Problem(EffectProblem.Of(
                EffectCodes.ProtocolVersion,
                $"Protocol version {batch.Version} is not this client's {EffectsProtocol.Version}."));
            return pass.Refused();
        }

        if (batch.Effects.Count > Limits.MaxEffects)
        {
            pass.Problem(EffectProblem.At(
                0,
                EffectCodes.LimitExceeded,
                string.Create(CultureInfo.InvariantCulture, $"A batch carries at most {Limits.MaxEffects} effects.")));
        }

        var flat = new List<(int Index, Effect Effect)>(batch.Effects.Count);
        int considered = Math.Min(batch.Effects.Count, Math.Max(Limits.MaxEffects, 0));
        for (int index = 0; index < considered; index++)
        {
            Flatten(batch.Effects[index], index, 0, flat, pass);
        }

        foreach ((int index, Effect effect) in flat)
        {
            try
            {
                ApplyOne(form, effect, index, pass);
            }
#pragma warning disable CA1031 // The protocol's promise is that no effect costs the rest of the round.
            catch (Exception)
#pragma warning restore CA1031
            {
                pass.Problem(EffectProblem.At(index, EffectCodes.EffectInvalid, "The effect could not be applied."));
            }
        }

        return pass.Build(form);
    }

    /// <summary>Applies the effect <paramref name="action"/> carries, for an action the user chose.</summary>
    public ApplyResult ApplyAction(FormState form, FormAction action)
    {
        ArgumentNullException.ThrowIfNull(form);
        ArgumentNullException.ThrowIfNull(action);
        return Apply(form, EffectBatch.Of(action.OnChosen));
    }

    private void Flatten(Effect effect, int index, int depth, List<(int Index, Effect Effect)> flat, ApplyResult.Builder pass)
    {
        if (depth > Limits.MaxBatchDepth)
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.LimitExceeded,
                string.Create(CultureInfo.InvariantCulture, $"A nested batch may be at most {Limits.MaxBatchDepth} levels deep.")));
            return;
        }

        if (effect is BatchEffect batch)
        {
            foreach (Effect child in batch.Effects)
            {
                Flatten(child, index, depth + 1, flat, pass);
            }

            return;
        }

        flat.Add((index, effect));
    }

    private void ApplyOne(FormState form, Effect effect, int index, ApplyResult.Builder pass)
    {
        switch (effect)
        {
            case SetValueEffect setValue:
                ApplySetValue(form, setValue, index, pass);
                break;
            case SetReadOnlyEffect setReadOnly:
                ApplyFlag(form, setReadOnly, setReadOnly.Path, index, pass, () => form.Presentation.SetReadOnly(setReadOnly.Path, setReadOnly.ReadOnly));
                break;
            case SetHiddenEffect setHidden:
                ApplyFlag(form, setHidden, setHidden.Path, index, pass, () => form.Presentation.SetHidden(setHidden.Path, setHidden.Hidden));
                break;
            case SetRequiredEffect setRequired:
                ApplyFlag(form, setRequired, setRequired.Path, index, pass, () => form.Presentation.SetRequired(setRequired.Path, setRequired.Required));
                break;
            case SetOptionsEffect setOptions:
                ApplySetOptions(form, setOptions, index, pass);
                break;
            case SetLabelEffect setLabel:
                ApplySetLabel(form, setLabel, index, pass);
                break;
            case AddActionEffect addAction:
                ApplyAddAction(form, addAction, index, pass);
                break;
            case RemoveActionEffect removeAction:
                ApplyRemoveAction(form, removeAction, index, pass);
                break;
            case ShowMessageEffect showMessage:
                ApplyShowMessage(form, showMessage, index, pass);
                break;
            case OpenDialogEffect openDialog:
                ApplyOpenDialog(form, openDialog, index, pass);
                break;
            case FocusEffect focus:
                ApplyFocus(form, focus, index, pass);
                break;
            case SetFilterEffect setFilter:
                ApplySetFilter(form, setFilter, index, pass);
                break;
            case BatchEffect:
                pass.Problem(EffectProblem.At(index, EffectCodes.EffectInvalid, "A batch is flattened before it is applied."));
                break;
            default:
                pass.Problem(EffectProblem.At(
                    index,
                    EffectCodes.EffectUnknown,
                    $"`{effect.Kind}` is not an effect this protocol version knows."));
                break;
        }
    }

    private static void ApplySetValue(FormState form, SetValueEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!Resolve(form, effect.Path, index, pass, out FieldTarget target))
        {
            return;
        }

        if (target.State is { IsMasked: true })
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.PathUnavailable, "An effect targets a field the user may not see."));
            return;
        }

        if (target.Field.ReadOnly
            || target.State is { IsEditable: false }
            || form.Presentation.IsReadOnly(target.Wire))
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.FieldNotWritable, "The user may not write this field.")
                .WithPath(target.Wire));
            return;
        }

        if (!EffectValueDecoder.TryDecode(target.Field, effect.ValueJson, out FieldValue value, out string why))
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.ValueInvalid,
                $"The value does not fit `{target.Wire}`: {why}.").WithPath(target.Wire));
            return;
        }

        form.TrySetValue(target, value);
        pass.Applied(effect.Kind, target.Wire);
    }

    private static void ApplyFlag(
        FormState form,
        Effect effect,
        string path,
        int index,
        ApplyResult.Builder pass,
        Action set)
    {
        if (!Resolve(form, path, index, pass, out FieldTarget target))
        {
            return;
        }

        set();
        form.NotePresentationChanged(target.Wire, effect.Kind);
        pass.Applied(effect.Kind, target.Wire);
    }

    private void ApplySetOptions(FormState form, SetOptionsEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!Resolve(form, effect.Path, index, pass, out FieldTarget target))
        {
            return;
        }

        FieldKind kind = target.Field.Kind;
        if (kind is not (FieldKind.Select or FieldKind.Link or FieldKind.DynamicLink))
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.OptionInvalid, "Candidate values apply to a select or link field only.")
                .WithPath(target.Wire));
            return;
        }

        if (effect.Options.Count > Limits.MaxOptions)
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.OptionInvalid,
                $"At most {Limits.MaxOptions} candidate values are allowed.").WithPath(target.Wire));
            return;
        }

        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (OptionItem option in effect.Options)
        {
            if (string.IsNullOrEmpty(option.Value))
            {
                pass.Problem(EffectProblem.At(index, EffectCodes.OptionInvalid, "A candidate value is empty.")
                    .WithPath(target.Wire));
                return;
            }

            if (!seen.Add(option.Value))
            {
                pass.Problem(EffectProblem.At(index, EffectCodes.OptionInvalid, "A candidate value appears twice.")
                    .WithPath(target.Wire));
                return;
            }

            if (option.Label is { } label && (label.Trim().Length == 0 || CountRunes(label) > Limits.MaxLabelChars))
            {
                pass.Problem(EffectProblem.At(
                    index,
                    EffectCodes.OptionInvalid,
                    $"A candidate label is empty or longer than {Limits.MaxLabelChars} characters.").WithPath(target.Wire));
                return;
            }

            if (kind == FieldKind.Select && !target.Field.Options.Contains(option.Value, StringComparer.Ordinal))
            {
                pass.Problem(EffectProblem.At(
                    index,
                    EffectCodes.OptionInvalid,
                    "A candidate value is not one of the select's values.").WithPath(target.Wire));
                return;
            }
        }

        form.Presentation.SetOptions(target.Wire, effect.Options);
        form.NotePresentationChanged(target.Wire, effect.Kind);
        pass.Applied(effect.Kind, target.Wire);
    }

    private void ApplySetLabel(FormState form, SetLabelEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!Resolve(form, effect.Path, index, pass, out FieldTarget target))
        {
            return;
        }

        if (effect.Label.Trim().Length == 0 || CountRunes(effect.Label) > Limits.MaxLabelChars)
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.LabelInvalid,
                $"A label is non-empty and at most {Limits.MaxLabelChars} characters.").WithPath(target.Wire));
            return;
        }

        form.Presentation.SetLabel(target.Wire, effect.Label);
        form.NotePresentationChanged(target.Wire, effect.Kind);
        pass.Applied(effect.Kind, target.Wire);
    }

    private void ApplyAddAction(FormState form, AddActionEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!IsId(effect.Id))
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.ActionInvalid, $"`{effect.Id}` is not an action id."));
            return;
        }

        if (effect.Label.Trim().Length == 0 || CountRunes(effect.Label) > Limits.MaxLabelChars)
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.ActionInvalid, "An action label is non-empty and short."));
            return;
        }

        form.AddOrReplaceAction(new FormAction(effect.Id, effect.Label, effect.Inner));
        pass.Applied(effect.Kind);
    }

    private static void ApplyRemoveAction(FormState form, RemoveActionEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!IsId(effect.Id))
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.ActionInvalid, $"`{effect.Id}` is not an action id."));
            return;
        }

        form.RemoveAction(effect.Id);
        pass.Applied(effect.Kind);
    }

    private void ApplyShowMessage(FormState form, ShowMessageEffect effect, int index, ApplyResult.Builder pass)
    {
        string? text = string.IsNullOrEmpty(effect.Text) ? null : effect.Text;
        string? textKey = string.IsNullOrEmpty(effect.TextKey) ? null : effect.TextKey;
        if (text is null && textKey is null)
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.MessageInvalid, "A message needs text or a catalogue key."));
            return;
        }

        if (text is not null && CountRunes(text) > Limits.MaxTextChars)
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.MessageInvalid,
                $"A message is at most {Limits.MaxTextChars} characters."));
            return;
        }

        if (textKey is not null && !IsTextKey(textKey))
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.MessageInvalid, "A catalogue key is dotted lower-case."));
            return;
        }

        string? field = null;
        if (effect.Field is { } fieldPath)
        {
            if (!Resolve(form, fieldPath, index, pass, out FieldTarget target))
            {
                return;
            }

            field = target.Wire;
        }

        form.AddMessage(new FormMessage(effect.Level, text, textKey, field));
        if (field is null)
        {
            pass.Applied(effect.Kind);
        }
        else
        {
            pass.Applied(effect.Kind, field);
        }
    }

    private void ApplyOpenDialog(FormState form, OpenDialogEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!IsId(effect.DialogId))
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.EffectInvalid, $"`{effect.DialogId}` is not a dialog id."));
            return;
        }

        if (RegisteredDialogs is not null && !RegisteredDialogs.Contains(effect.DialogId))
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.DialogUnknown,
                $"`{effect.DialogId}` is not a dialog this form registers."));
            return;
        }

        form.RequestDialog(new DialogIntent(effect.DialogId, effect.ParamsJson));
        pass.Applied(effect.Kind);
    }

    private static void ApplyFocus(FormState form, FocusEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!Resolve(form, effect.Path, index, pass, out FieldTarget target))
        {
            return;
        }

        form.SetFocus(target.Wire);
        pass.Applied(effect.Kind, target.Wire);
    }

    private void ApplySetFilter(FormState form, SetFilterEffect effect, int index, ApplyResult.Builder pass)
    {
        if (!Resolve(form, effect.LinkPath, index, pass, out FieldTarget target))
        {
            return;
        }

        if (target.Field.Kind is not (FieldKind.Link or FieldKind.DynamicLink))
        {
            pass.Problem(EffectProblem.At(index, EffectCodes.FilterInvalid, "A filter applies to a link field only.")
                .WithPath(target.Wire));
            return;
        }

        if (CountRunes(effect.Filter) > Limits.MaxFilterChars)
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.FilterInvalid,
                $"A filter is at most {Limits.MaxFilterChars} characters.").WithPath(target.Wire));
            return;
        }

        try
        {
            Spotledger.Client.Expressions.Expression.Parse(effect.Filter);
        }
        catch (Spotledger.Client.Expressions.ExpressionParseException error)
        {
            pass.Problem(EffectProblem.At(
                index,
                EffectCodes.FilterInvalid,
                $"The filter does not parse: {error.Code}.").WithPath(target.Wire));
            return;
        }

        form.Presentation.SetFilter(target.Wire, effect.Filter);
        form.NotePresentationChanged(target.Wire, effect.Kind);
        pass.Applied(effect.Kind, target.Wire);
    }

    private static bool Resolve(FormState form, string path, int index, ApplyResult.Builder pass, out FieldTarget target)
    {
        if (!form.TryResolve(path, out target, out EffectProblem? problem))
        {
            pass.Problem((problem ?? FormResolution.Unknown(path)).WithIndex(index));
            return false;
        }

        return true;
    }

    private static int CountRunes(string text)
    {
        int count = 0;
        foreach (Rune _ in text.EnumerateRunes())
        {
            count++;
        }

        return count;
    }

    /// <summary>An action or dialog id: lowercase start, then <c>[a-z0-9_.:-]</c>, at most 64 characters.</summary>
    private static bool IsId(string? text)
    {
        if (string.IsNullOrEmpty(text) || text.Length > 64)
        {
            return false;
        }

        if (text[0] < 'a' || text[0] > 'z')
        {
            return false;
        }

        foreach (char c in text)
        {
            bool ok = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c is '_' or '-' or '.' or ':';
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>A message catalogue key: dotted lower-case, dashes and underscores allowed.</summary>
    private static bool IsTextKey(string key)
    {
        if (key.Length == 0)
        {
            return false;
        }

        foreach (char c in key)
        {
            bool ok = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c is '.' or '_' or '-';
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }
}
