using System.Text;
using System.Text.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms;

public sealed partial class DocumentFormViewModel
{
    /// <summary>The toolbar actions, with whether each can run now. Enabled by the permission
    /// summary and the document state; the server still decides every one.</summary>
    public IReadOnlyList<FormAction> Actions => BuildActions();

    private void RaiseActionsChanged()
    {
        OnPropertyChanged(nameof(Actions));
        OnPropertyChanged(nameof(IsReadOnly));
    }

    private IReadOnlyList<FormAction> BuildActions()
    {
        if (_draft is null)
        {
            return [];
        }

        var posted = _draft.Model.IsPosted;
        var existing = !IsNew;
        var busy = _busy;
        var conflicted = _conflict is not null;

        string? Why(bool allowed, string denied) => allowed ? null : denied;

        FormAction Make(string id, string label, string? reason, string? gesture) =>
            new(id, label, reason is null && !busy && !conflicted, busy ? "A request is in flight." : conflicted ? "Resolve the version conflict first." : reason, gesture);

        var saveReason = existing
            ? !_rights.Write ? "You may not change this document."
                : Status is DocumentStatus.Posted or DocumentStatus.Cancelled ? "A submitted or cancelled document cannot be edited."
                : null
            : !_rights.Create ? "You may not create this document." : null;
        return
        [
            Make("save", "Save", saveReason, "Ctrl+S"),
            Make("submit", "Submit",
                !posted ? "This document type is not submitted."
                    : !existing ? "Save the document first."
                    : !_rights.Submit ? "You may not submit this document."
                    : Status is not DocumentStatus.Draft ? "Only a draft can be submitted." : null, "Ctrl+Enter"),
            Make("cancel", "Cancel document",
                !posted ? "This document type is not submitted."
                    : !_rights.Cancel ? "You may not cancel this document."
                    : Status is not DocumentStatus.Posted ? "Only a submitted document can be cancelled." : null, null),
            Make("correct", "Correct",
                !posted ? "This document type is not submitted."
                    : !_rights.Correct ? "You may not correct this document."
                    : Status is not DocumentStatus.Posted ? "Only a submitted document can be corrected." : null, null),
            Make("delete", "Delete",
                Why(existing, "A new document has nothing to delete.")
                    ?? Why(_rights.Delete, "You may not delete this document.")
                    ?? Why(Status is not DocumentStatus.Posted, "A submitted document cannot be deleted; cancel it first."), null),
            Make("rename", "Rename",
                Why(existing, "A new document has no name to change.")
                    ?? Why(_rights.Rename, "You may not rename this document."), null),
            Make("new", "New", Why(_rights.Create, "You may not create this document."), "Ctrl+N"),
        ];
    }

    /// <summary>Runs the action called <paramref name="id"/> when it is enabled.</summary>
    public Task<bool> RunActionAsync(string id)
    {
        // A value typed into an editor that has not lost focus yet is not in the draft: commit
        // the editors first, so the state the actions are judged on is what the person sees.
        CommitEditors?.Invoke();
        if (!Actions.Any(action => action.Id == id && action.IsEnabled))
        {
            return Task.FromResult(false);
        }

        return id switch
        {
            "save" => SaveAsync(),
            "submit" => SubmitAsync(),
            "cancel" => CancelDocumentAsync(),
            "correct" => CorrectAsync(),
            "delete" => DeleteAsync(),
            "rename" => RenameAsync(),
            "new" => NewAsync(),
            _ => Task.FromResult(false),
        };
    }

    // ------------------------------------------------------------------ save

    /// <summary>Validates, then creates (a new document) or sends a merge patch of the changed
    /// fields against the loaded version. Returns whether the document was saved.</summary>
    public async Task<bool> SaveAsync()
    {
        var draft = _draft;
        if (draft is null || _busy)
        {
            return false;
        }

        if (CommitEditors is not null && !CommitEditors())
        {
            FocusFirstInvalid();
            return false;
        }

        if (!ValidateLocally(draft))
        {
            return false;
        }

        if (!IsNew && !draft.IsDirty)
        {
            return true;
        }

        return await SendAsync(overrideVersion: null);
    }

    private bool ValidateLocally(DocumentDraft draft)
    {
        foreach (var state in draft.Fields.Values)
        {
            state.ClearErrors();
        }

        var problems = DraftValidator.Validate(draft);
        if (problems.Count == 0)
        {
            return true;
        }

        var error = new ApiError
        {
            Code = "validation.failed",
            Message = "The document is not valid.",
            FieldErrors = problems.Select(p => new ApiFieldError(p.Field, p.Code, p.Message)).ToArray(),
        };
        Messages.Clear();
        ShowError(error);
        return false;
    }

    private async Task<bool> SendAsync(int? overrideVersion)
    {
        var draft = _draft!;
        IsBusy = true;
        try
        {
            DocumentReply reply;
            if (draft.DocumentId is null)
            {
                reply = await _gateway.CreateAsync(_doctype, BuildCreateBody(draft), _lifetime.Token);
            }
            else
            {
                reply = await _gateway.PatchAsync(
                    _doctype, draft.DocumentId, draft.ToPatch(), overrideVersion ?? draft.Version, _lifetime.Token);
            }

            ApplyReply(reply);
            return true;
        }
        catch (ApiRequestException e)
        {
            var applied = ShowError(e.Error);
            if (applied.IsStaleVersion)
            {
                return await HandleConflictAsync(applied.CurrentVersion);
            }

            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>The body of a create: every changed field and table, plus the defaults a new
    /// draft started with, so a server that does not repeat the metadata's defaults still gets them.</summary>
    private static string BuildCreateBody(DocumentDraft draft)
    {
        using var patch = JsonDocument.Parse(draft.ToPatch());
        using var snapshot = JsonDocument.Parse(draft.LookupSnapshot().Doc);
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            var written = new HashSet<string>(StringComparer.Ordinal);
            foreach (var property in patch.RootElement.EnumerateObject())
            {
                property.WriteTo(writer);
                written.Add(property.Name);
            }

            foreach (var property in snapshot.RootElement.EnumerateObject())
            {
                if (written.Contains(property.Name)
                    || property.Value.ValueKind == JsonValueKind.Null
                    || property.Value is { ValueKind: JsonValueKind.String } text && text.GetString() is { Length: 0 }
                    || !draft.Fields.TryGetValue(property.Name, out var state)
                    || state.IsReadOnly || !state.IsEditable || state.IsMasked)
                {
                    continue;
                }

                property.WriteTo(writer);
            }

            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    // ------------------------------------------------------------------ conflicts

    private async Task<bool> HandleConflictAsync(int? currentVersion)
    {
        var draft = _draft!;
        var info = new ConflictInfo(draft.DocumentId, draft.Version, currentVersion, draft.DirtyPaths());
        Conflict = info;
        var choice = await Shell.ChooseConflictAsync(info);
        return await ResolveConflictAsync(choice);
    }

    /// <summary>Resolves the pending version conflict. A conflict is never settled silently:
    /// this runs only on an explicit choice.</summary>
    public async Task<bool> ResolveConflictAsync(ConflictChoice choice)
    {
        var draft = _draft;
        var info = _conflict;
        if (draft?.DocumentId is not { } id || info is null)
        {
            return false;
        }

        switch (choice)
        {
            case ConflictChoice.ReloadAndReapply:
                await ReloadAndReapplyAsync(id);
                return false;
            case ConflictChoice.Overwrite:
                {
                    var version = info.CurrentVersion ?? await FetchVersionAsync(id);
                    if (version is null)
                    {
                        return false;
                    }

                    Conflict = null;
                    return await SendAsync(version);
                }

            default:
                return false;
        }
    }

    private async Task<int?> FetchVersionAsync(string id)
    {
        try
        {
            var reply = await _gateway.LoadAsync(_doctype, id, _lifetime.Token);
            using var document = JsonDocument.Parse(reply.DataJson);
            return document.RootElement.TryGetProperty("version", out var version) && version.TryGetInt32(out var number) ? number : null;
        }
        catch (ApiRequestException e)
        {
            AddMessage("error", e.Error.Code, e.Error.Message);
            return null;
        }
    }

    private async Task ReloadAndReapplyAsync(string id)
    {
        var draft = _draft!;
        var scalars = draft.Fields
            .Where(pair => pair.Value.IsDirty && !pair.Value.IsMasked && !draft.Tables.ContainsKey(pair.Key))
            .ToDictionary(pair => pair.Key, pair => pair.Value.Value, StringComparer.Ordinal);
        var tables = draft.Tables
            .Where(pair => pair.Value.IsDirty)
            .ToDictionary(
                pair => pair.Key,
                pair => pair.Value.Rows
                    .Select(row => (row.Id, Values: row.Values.Where(v => !v.Value.IsMasked).ToDictionary(v => v.Key, v => v.Value.Value)))
                    .ToList(),
                StringComparer.Ordinal);
        IsBusy = true;
        try
        {
            var reply = await _gateway.LoadAsync(_doctype, id, _lifetime.Token);
            draft.LoadFrom(reply.DataJson);
            Audit = AuditFooter.From(reply.DataJson);
            foreach (var (name, value) in scalars)
            {
                draft.Set(name, value);
            }

            foreach (var (name, rows) in tables)
            {
                var table = draft.Table(name);
                while (table.Rows.Count > 0)
                {
                    table.RemoveRowAt(0);
                }

                foreach (var (rowId, values) in rows)
                {
                    var row = table.AddRow();
                    foreach (var (field, value) in values)
                    {
                        row.SetValue(field, value);
                    }

                    if (rowId is not null)
                    {
                        table.AssignRowId(row, rowId);
                    }
                }
            }

            Conflict = null;
            AddMessage("info", null, "The latest version was loaded and your changes were applied on top of it. Review them, then save.");
            Rebuild();
            RaiseAll();
        }
        catch (ApiRequestException e)
        {
            AddMessage("error", e.Error.Code, e.Error.Message);
        }
        finally
        {
            IsBusy = false;
        }
    }

    // ------------------------------------------------------------------ lifecycle

    /// <summary>Submits (posts) the document, saving pending changes first, after a confirmation.</summary>
    public async Task<bool> SubmitAsync()
    {
        var draft = _draft;
        if (draft?.DocumentId is not { } id || !CanRun("submit"))
        {
            return false;
        }

        if (draft.IsDirty && !await SaveAsync())
        {
            return false;
        }

        if (!await Shell.ConfirmAsync($"Submit {draft.Model.Label}", "A submitted document can no longer be edited. Submit it now?", "Submit"))
        {
            return false;
        }

        return await LifecycleAsync(LifecycleAction.Submit, id, reason: null, "Submit");
    }

    /// <summary>Cancels a submitted document, asking for a reason when the DocType requires one.</summary>
    public async Task<bool> CancelDocumentAsync()
    {
        var draft = _draft;
        if (draft?.DocumentId is not { } id || !CanRun("cancel"))
        {
            return false;
        }

        string? reason = null;
        if (RequiresCancelReason(draft.Model))
        {
            reason = await Shell.AskTextAsync($"Cancel {draft.Model.Label}", "Why is this document being cancelled?", required: true);
            if (string.IsNullOrWhiteSpace(reason))
            {
                return false;
            }
        }
        else if (!await Shell.ConfirmAsync($"Cancel {draft.Model.Label}", "Cancel this submitted document?", "Cancel document"))
        {
            return false;
        }

        return await LifecycleAsync(LifecycleAction.Cancel, id, reason, "Cancel");
    }

    /// <summary>Cancels and amends a submitted document; the amended draft opens in its own tab.</summary>
    public async Task<bool> CorrectAsync()
    {
        var draft = _draft;
        if (draft?.DocumentId is not { } id || !CanRun("correct"))
        {
            return false;
        }

        if (!await Shell.ConfirmAsync($"Correct {draft.Model.Label}", "This cancels the submitted document and opens an amended draft. Continue?", "Correct"))
        {
            return false;
        }

        IsBusy = true;
        try
        {
            var reply = await _gateway.LifecycleAsync(_doctype, id, LifecycleAction.Correct, draft.Version, null, _lifetime.Token);
            using var document = JsonDocument.Parse(reply.DataJson);
            var amended = document.RootElement.TryGetProperty("id", out var value) ? value.GetString() : null;
            if (amended is not null && amended != id)
            {
                // The answer is the amended draft: it opens beside this document, which is
                // reloaded to show it cancelled.
                Shell.OpenDocument(_doctype, amended);
                await ReloadAsync(id);
            }
            else
            {
                ApplyReply(reply);
            }

            return true;
        }
        catch (ApiRequestException e)
        {
            await ShowActionErrorAsync(e.Error, "correct");
            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }

    private async Task ReloadAsync(string id)
    {
        try
        {
            ApplyReply(await _gateway.LoadAsync(_doctype, id, _lifetime.Token));
        }
        catch (ApiRequestException e)
        {
            AddMessage("error", e.Error.Code, e.Error.Message);
        }
    }

    /// <summary>Deletes the document after a confirmation; a referenced document lists its referrers.</summary>
    public async Task<bool> DeleteAsync()
    {
        var draft = _draft;
        if (draft?.DocumentId is not { } id || !CanRun("delete"))
        {
            return false;
        }

        if (!await Shell.ConfirmAsync($"Delete {draft.Model.Label}", "Delete this document? This cannot be undone.", "Delete"))
        {
            return false;
        }

        IsBusy = true;
        try
        {
            await _gateway.DeleteAsync(_doctype, id, draft.Version, _lifetime.Token);
            Autosave?.DraftSettled(_doctype, id);
            draft.MarkSaved();
            RaiseDirty();
            RequestClose();
            return true;
        }
        catch (ApiRequestException e)
        {
            await ShowActionErrorAsync(e.Error, "delete");
            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>Changes the document's name after asking for the new one.</summary>
    public async Task<bool> RenameAsync()
    {
        var draft = _draft;
        if (draft?.DocumentId is not { } id || !CanRun("rename"))
        {
            return false;
        }

        var name = await Shell.AskTextAsync($"Rename {draft.Model.Label}", "New name", required: true);
        if (string.IsNullOrWhiteSpace(name))
        {
            return false;
        }

        IsBusy = true;
        try
        {
            var reply = await _gateway.RenameAsync(_doctype, id, name.Trim(), draft.Version, _lifetime.Token);
            ApplyReply(reply);
            return true;
        }
        catch (ApiRequestException e)
        {
            await ShowActionErrorAsync(e.Error, "rename");
            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>Opens a new document of this DocType in its own tab.</summary>
    public Task<bool> NewAsync()
    {
        if (!CanRun("new"))
        {
            return Task.FromResult(false);
        }

        Shell.OpenDocument(_doctype, null);
        return Task.FromResult(true);
    }

    /// <summary>Opens the document a link field points to (Ctrl+Click or the open glyph).</summary>
    public void OpenLink(string field)
    {
        var model = _draft?.Model.Field(field);
        if (model?.LinkTo is { } target && _draft?.Get(field) is LinkFieldValue { Id: { } id })
        {
            Shell.OpenDocument(target, id);
        }
    }

    private bool CanRun(string id) => Actions.Any(action => action.Id == id && action.IsEnabled);

    private async Task<bool> LifecycleAsync(LifecycleAction action, string id, string? reason, string verb)
    {
        IsBusy = true;
        try
        {
            var reply = await _gateway.LifecycleAsync(_doctype, id, action, _draft!.Version, reason, _lifetime.Token);
            ApplyReply(reply);
            AddMessage("info", null, verb switch
            {
                "Submit" => "The document was submitted.",
                "Cancel" => "The document was cancelled.",
                _ => verb + " done.",
            });
            return true;
        }
        catch (ApiRequestException e)
        {
            await ShowActionErrorAsync(e.Error, verb.ToLowerInvariant());
            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>Shows a failed lifecycle action: referenced documents as a list, a stale version as
    /// the conflict panel, anything else as field errors and messages.</summary>
    private async Task ShowActionErrorAsync(ApiError error, string verb)
    {
        if (error.Code.EndsWith("referenced", StringComparison.Ordinal))
        {
            var referrers = error.FieldErrors
                .Select(f => new Referrer(string.IsNullOrEmpty(f.Field) ? f.Message : f.Field, string.IsNullOrEmpty(f.Field) ? null : f.Message))
                .ToList();
            AddMessage("error", error.Code, error.Message);
            if (referrers.Count > 0)
            {
                await Shell.ShowReferrersAsync($"Cannot {verb}: still referenced", referrers);
            }

            return;
        }

        var applied = ShowError(error);
        if (applied.IsStaleVersion)
        {
            await HandleConflictAsync(applied.CurrentVersion);
        }
    }

    // ------------------------------------------------------------------ unsaved changes

    /// <summary>What is not saved, as the field paths that changed; empty when the draft is clean.</summary>
    public IReadOnlyList<string> UnsavedChanges => _draft?.DirtyPaths() ?? [];

    /// <summary>The guard for closing the tab or the window: true when nothing is lost, or when
    /// the person agreed to discard the listed changes.</summary>
    public async Task<bool> RequestCloseAsync()
    {
        if (_draft is null || !_draft.IsDirty)
        {
            return true;
        }

        var discard = await Shell.ConfirmDiscardAsync(_draft.DirtyPaths());
        if (discard)
        {
            Autosave?.DraftSettled(_doctype, _draft.DocumentId);
        }

        return discard;
    }

    /// <summary>Navigates this form to another document of the same DocType, guarded like a close.</summary>
    public async Task<bool> NavigateAsync(string id)
    {
        if (!await RequestCloseAsync())
        {
            return false;
        }

        await ReloadAsync(id);
        return true;
    }
}
