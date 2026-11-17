using System.Globalization;
using System.Text.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Forms;

/// <summary>Which value a person keeps for a field that changed on the server and in their edits.</summary>
public enum MergeSide
{
    /// <summary>Keep the value the person typed.</summary>
    Mine,

    /// <summary>Take the value the server now holds.</summary>
    Theirs,
}

/// <summary>
/// One field (or child table) that was changed on the server and by the person. Both values are shown and
/// the person picks; <see cref="Side"/> starts as <see cref="MergeSide.Mine"/>, so the server's value is
/// never applied over an edited field unless the person chooses it.
/// </summary>
public sealed class MergeChoice : ObservableObject
{
    private MergeSide _side = MergeSide.Mine;

    internal MergeChoice(string field, string label, string mine, string theirs, object theirsData)
    {
        Field = field;
        Label = label;
        Mine = mine;
        Theirs = theirs;
        TheirsData = theirsData;
    }

    /// <summary>The field (or table) name.</summary>
    public string Field { get; }

    /// <summary>The label to show.</summary>
    public string Label { get; }

    /// <summary>The person's value, as text.</summary>
    public string Mine { get; }

    /// <summary>The server's value, as text.</summary>
    public string Theirs { get; }

    /// <summary>The chosen side.</summary>
    public MergeSide Side
    {
        get => _side;
        set => SetField(ref _side, value);
    }

    internal object TheirsData { get; }
}

/// <summary>The fields waiting for the person's choice after a merge.</summary>
/// <param name="Conflicts">The fields changed on both sides, in field order.</param>
public sealed record MergeProposal(IReadOnlyList<MergeChoice> Conflicts);

public sealed partial class DocumentFormViewModel
{
    private MergeProposal? _pendingMerge;

    /// <summary>The merge waiting for the person's choices, or null. While it is set the toolbar actions are
    /// disabled so a save cannot send a value the person has not settled.</summary>
    public MergeProposal? PendingMerge
    {
        get => _pendingMerge;
        private set
        {
            if (SetField(ref _pendingMerge, value))
            {
                RaiseActionsChanged();
            }
        }
    }

    private sealed record RowSnap(string? Id, Dictionary<string, FieldValue> Values);

    /// <summary>
    /// Merges the person's unsaved edits with the server's current document: loads it, keeps every edited
    /// field and child table, takes the server's value for everything the person did not edit (silently),
    /// and lists each field changed on the server <em>and</em> by the person as a <see cref="PendingMerge"/>
    /// conflict with both values. Nothing from the server replaces an edited field until
    /// <see cref="ApplyMergeChoices"/> runs with the person's choice. The draft then holds the server's
    /// version, so a save sends it as the base version. Returns false when nothing was merged (no document,
    /// busy, a merge already waiting, an editor that does not parse, or a failed load).
    /// </summary>
    /// <remarks>
    /// The form model has no per-field history of the server's side, so "changed on the server" is judged
    /// against the values the form was loaded with: the draft is reverted to them to read the base, then the
    /// server's document is loaded and the person's edits are put back on top.
    /// </remarks>
    public async Task<bool> MergeServerChangesAsync()
    {
        var draft = _draft;
        if (draft?.DocumentId is not { } id || _busy || _pendingMerge is not null)
        {
            return false;
        }

        if (CommitEditors is not null && !CommitEditors())
        {
            FocusFirstInvalid();
            return false;
        }

        IsBusy = true;
        try
        {
            var reply = await _gateway.LoadAsync(_doctype, id, _lifetime.Token);
            using (JsonDocument.Parse(reply.DataJson))
            {
                // A body that does not parse fails here, before the edits are touched.
            }

            var mineScalars = draft.Fields
                .Where(pair => pair.Value.IsDirty && !pair.Value.IsMasked && !draft.Tables.ContainsKey(pair.Key))
                .ToDictionary(pair => pair.Key, pair => pair.Value.Value, StringComparer.Ordinal);
            var mineTables = draft.Tables
                .Where(pair => pair.Value.IsDirty)
                .ToDictionary(pair => pair.Key, pair => Snapshot(pair.Value), StringComparer.Ordinal);

            draft.Revert();
            var baseScalars = mineScalars.Keys.ToDictionary(name => name, name => draft.Fields[name].Value, StringComparer.Ordinal);
            var baseTables = mineTables.Keys.ToDictionary(name => name, name => Snapshot(draft.Table(name)), StringComparer.Ordinal);
            draft.LoadFrom(reply.DataJson);
            Audit = AuditFooter.From(reply.DataJson);

            var conflicts = new List<MergeChoice>();
            foreach (var field in draft.Model.Fields)
            {
                if (mineScalars.TryGetValue(field.Name, out var mine))
                {
                    var theirs = draft.Fields[field.Name].Value;
                    if (SameValue(mine, theirs))
                    {
                        continue;
                    }

                    if (!SameValue(baseScalars[field.Name], theirs))
                    {
                        conflicts.Add(new MergeChoice(field.Name, field.Label, Show(mine), Show(theirs), theirs));
                    }

                    draft.Set(field.Name, mine);
                }
                else if (mineTables.TryGetValue(field.Name, out var mineRows))
                {
                    var table = draft.Table(field.Name);
                    var theirRows = Snapshot(table);
                    if (SameRows(mineRows, theirRows))
                    {
                        continue;
                    }

                    if (!SameRows(baseTables[field.Name], theirRows))
                    {
                        conflicts.Add(new MergeChoice(field.Name, field.Label, Describe(mineRows), Describe(theirRows), theirRows));
                    }

                    ApplyRows(table, mineRows);
                }
            }

            Conflict = null;
            AddMessage("info", null, conflicts.Count == 0
                ? "The latest version was loaded and your changes were kept on top of it. Review them, then save."
                : $"The latest version was loaded. {conflicts.Count} changed on the server and in your edits: choose which value to keep for each, then save.");
            PendingMerge = conflicts.Count > 0 ? new MergeProposal(conflicts) : null;
            Rebuild();
            RaiseAll();
            return true;
        }
        catch (ApiRequestException e)
        {
            AddMessage("error", e.Error.Code, e.Error.Message);
            return false;
        }
        catch (JsonException e)
        {
            AddMessage("error", null, "The document could not be read: " + e.Message);
            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>
    /// Settles the pending merge: every field the person marked <see cref="MergeSide.Theirs"/> takes the
    /// server's value, every other keeps the person's. Nothing is sent; saving does that.
    /// </summary>
    /// <returns>Whether there was a merge to settle.</returns>
    public bool ApplyMergeChoices()
    {
        var draft = _draft;
        var proposal = _pendingMerge;
        if (draft is null || proposal is null)
        {
            return false;
        }

        foreach (var choice in proposal.Conflicts.Where(c => c.Side == MergeSide.Theirs))
        {
            switch (choice.TheirsData)
            {
                case FieldValue value:
                    draft.Set(choice.Field, value);
                    break;
                case List<RowSnap> rows:
                    ApplyRows(draft.Table(choice.Field), rows);
                    break;
            }
        }

        PendingMerge = null;
        Rebuild();
        RaiseAll();
        return true;
    }

    private static List<RowSnap> Snapshot(DocumentDraft.TableDraft table) =>
        table.Rows
            .Select(row => new RowSnap(
                row.Id,
                row.Values.Where(v => !v.Value.IsMasked).ToDictionary(v => v.Key, v => v.Value.Value, StringComparer.Ordinal)))
            .ToList();

    private static void ApplyRows(DocumentDraft.TableDraft table, List<RowSnap> rows)
    {
        while (table.Rows.Count > 0)
        {
            table.RemoveRowAt(0);
        }

        foreach (var row in rows)
        {
            var added = table.AddRow();
            foreach (var (field, value) in row.Values)
            {
                added.SetValue(field, value);
            }

            if (row.Id is not null)
            {
                table.AssignRowId(added, row.Id);
            }
        }
    }

    /// <summary>Exact comparison of two values by the draft's own rule (a changed scale of one number is the same).</summary>
    private static bool SameValue(FieldValue a, FieldValue b)
    {
        var probe = new FieldState(a) { Value = b };
        return !probe.IsDirty;
    }

    private static bool SameRows(List<RowSnap> a, List<RowSnap> b)
    {
        if (a.Count != b.Count)
        {
            return false;
        }

        for (var i = 0; i < a.Count; i++)
        {
            if (a[i].Id != b[i].Id || a[i].Values.Count != b[i].Values.Count)
            {
                return false;
            }

            foreach (var (name, value) in a[i].Values)
            {
                if (!b[i].Values.TryGetValue(name, out var other) || !SameValue(value, other))
                {
                    return false;
                }
            }
        }

        return true;
    }

    private static string Describe(List<RowSnap> rows) => rows.Count == 1 ? "1 row" : $"{rows.Count.ToString(CultureInfo.InvariantCulture)} rows";

    private static string Show(FieldValue value)
    {
        var text = value switch
        {
            TextFieldValue plain => plain.Value,
            IntFieldValue whole => whole.Value.ToString(CultureInfo.InvariantCulture),
            DecimalFieldValue number => number.CanonicalText,
            BoolFieldValue flag => flag.Value ? "Yes" : "No",
            SelectFieldValue option => option.Value,
            MultiSelectFieldValue options => string.Join(", ", options.Values),
            DateFieldValue day => day.Value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture),
            TimeFieldValue time => time.Value.ToString("HH:mm:ss", CultureInfo.InvariantCulture),
            DateTimeFieldValue instant => instant.Value.ToString("yyyy-MM-dd HH:mm:ss zzz", CultureInfo.InvariantCulture),
            LinkFieldValue link => link.DisplayText ?? link.Id ?? string.Empty,
            AttachFieldValue file => file.FileId ?? string.Empty,
            SecretFieldValue secret => secret.IsSet ? "Set" : "Not set",
            JsonFieldValue json => json.JsonText,
            _ => string.Empty,
        };
        return text.Length == 0 ? "(empty)" : text;
    }
}
