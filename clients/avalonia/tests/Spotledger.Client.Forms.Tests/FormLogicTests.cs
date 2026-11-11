using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

public sealed class FormLogicTests
{
    private static ApiError FieldErrors(params (string Field, string Message)[] errors) => new()
    {
        StatusCode = 422,
        Code = "document.validation_failed",
        Message = "The document is not valid.",
        FieldErrors = errors.Select(e => new ApiFieldError(e.Field, "document.invalid_value", e.Message)).ToArray(),
    };

    [Fact]
    public async Task OpenEditSaveRoundTripSendsAMergePatchAndTakesServerDerivedFieldsBack()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        gateway.Derived["amount"] = "99.00";
        var form = await Fixtures.OpenAsync(gateway, "INV-1");

        Assert.Equal(FormPhase.Ready, form.Phase);
        Assert.Equal("Invoice First invoice", form.Title);
        Assert.Equal("Draft", form.StatusBadge);
        Assert.False(form.IsDirty);
        Assert.Equal("ama", form.Audit.CreatedBy);
        Assert.Equal("ben", form.Audit.ModifiedBy);

        var draft = form.Draft!;
        Assert.True(draft.TrySetText("title", "Renamed"));
        var table = draft.Table("items");
        var row = table.AddRow();
        row.SetValue("item", new TextFieldValue("Cap"));
        row.SetValue("qty", ValueCodec.DecodeDecimalString("3.00"));
        Assert.True(form.IsDirty);
        Assert.Equal("Draft, modified", form.StatusBadge);

        Assert.True(await form.SaveAsync());

        Assert.Equal(["meta", "permissions", "load INV-1", "patch INV-1 @1"], gateway.Calls.Take(4));
        var patch = gateway.Bodies.Single();
        Assert.Contains("\"title\":\"Renamed\"", patch, StringComparison.Ordinal);
        Assert.DoesNotContain("\"amount\"", patch, StringComparison.Ordinal);
        Assert.Contains("\"items\"", patch, StringComparison.Ordinal);
        Assert.Contains("\"r1\"", patch, StringComparison.Ordinal);
        Assert.False(form.IsDirty);
        Assert.Equal(2, draft.Version);
        Assert.Equal("99.00", draft.Get("amount") is DecimalFieldValue amount ? amount.CanonicalText : null);
        Assert.Equal(3, draft.Table("items").Rows.Count);
    }

    [Fact]
    public async Task ServerFieldErrorsLandOnTheFieldAndTheCellAndTheFirstInvalidFieldIsFocused()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        var focused = new List<string>();
        form.FocusRequested += (_, field) => focused.Add(field);
        form.Draft!.TrySetText("title", "Changed");
        form.Draft.Table("items").Rows[1].SetValue("qty", ValueCodec.DecodeDecimalString("5.00"));
        gateway.RejectNextWrite = FieldErrors(("title", "Title is taken"), ("items[2].qty", "Too many"), ("", "Nope"));

        Assert.False(await form.SaveAsync());

        Assert.Equal("Title is taken", form.Draft.Fields["title"].Errors.Single().Message);
        Assert.Equal("Too many", form.Draft.Table("items").Rows[1].Values["qty"].Errors.Single().Message);
        Assert.Equal(["title"], focused);
        Assert.Contains(form.Messages, m => m.Text == "Nope" && m.Level == "error");
        Assert.True(form.IsDirty, "a refused save keeps the edits");
    }

    [Fact]
    public async Task ARequiredFieldIsCaughtBeforeTheRequestLeaves()
    {
        var gateway = new FakeGateway();
        var form = await Fixtures.OpenAsync(gateway, null);
        var focused = new List<string>();
        form.FocusRequested += (_, field) => focused.Add(field);

        Assert.False(await form.SaveAsync());

        Assert.DoesNotContain(gateway.Calls, c => c == "create");
        Assert.Equal(["title"], focused);
        Assert.NotEmpty(form.Draft!.Fields["title"].Errors);
    }

    [Fact]
    public async Task ACreateStartsFromDefaultsSendsThemAndAdoptsTheNewId()
    {
        var gateway = new FakeGateway();
        var form = await Fixtures.OpenAsync(gateway, null);
        var idChanges = 0;
        form.DocumentIdChanged += (_, _) => idChanges++;

        Assert.True(form.IsNew);
        Assert.Equal("New Invoice", form.Title);
        Assert.Equal("Not saved", form.StatusBadge);
        Assert.Equal("Normal", (form.Draft!.Get("priority") as SelectFieldValue)?.Value);
        form.Draft.TrySetText("title", "Brand new");

        Assert.True(await form.SaveAsync());

        var body = gateway.Bodies.Single();
        Assert.Contains("\"title\":\"Brand new\"", body, StringComparison.Ordinal);
        Assert.Contains("\"priority\":\"Normal\"", body, StringComparison.Ordinal);
        Assert.False(form.IsNew);
        Assert.StartsWith("NEW-", form.DocumentId, StringComparison.Ordinal);
        Assert.Equal(1, idChanges);
        Assert.Equal("document:invoice:" + form.DocumentId, form.TabKey);
        Assert.False(form.IsDirty);
    }

    [Fact]
    public async Task AStaleVersionShowsTheConflictAndNeverOverwritesSilently()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var shell = new FakeShell { Choice = ConflictChoice.Keep };
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);
        form.Draft!.TrySetText("title", "Mine");
        gateway.OtherPersonChanges("INV-1", "Theirs");

        Assert.False(await form.SaveAsync());

        Assert.NotNull(form.Conflict);
        Assert.Equal(1, form.Conflict!.YourVersion);
        Assert.Contains("title", form.Conflict.Changed);
        Assert.Contains("conflict", shell.Asked);
        Assert.Equal("Theirs", gateway.Documents["INV-1"]["title"]!.GetValue<string>());
        Assert.Single(gateway.Calls, c => c.StartsWith("patch", StringComparison.Ordinal));
        Assert.All(form.Actions, a => Assert.False(a.IsEnabled));
    }

    [Fact]
    public async Task ReloadAndReapplyKeepsMyEditsOnTopOfTheLatestVersionThenSaves()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        gateway.Documents["INV-1"]["note"] = "x";
        var shell = new FakeShell { Choice = ConflictChoice.ReloadAndReapply };
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);
        form.Draft!.TrySetText("title", "Mine");
        form.Draft.Table("items").AddRow().SetValue("qty", ValueCodec.DecodeDecimalString("7.00"));
        gateway.OtherPersonChanges("INV-1", "Theirs");
        gateway.Documents["INV-1"]["amount"] = "50.00";

        Assert.False(await form.SaveAsync());

        Assert.Null(form.Conflict);
        Assert.Equal(2, form.Draft.Version);
        Assert.Equal("Mine", (form.Draft.Get("title") as TextFieldValue)?.Value);
        Assert.Equal("50.00", (form.Draft.Get("amount") as DecimalFieldValue)?.CanonicalText);
        Assert.Equal(3, form.Draft.Table("items").Rows.Count);
        Assert.Equal("r1", form.Draft.Table("items").Rows[0].Id);
        Assert.True(form.IsDirty);

        Assert.True(await form.SaveAsync());
        Assert.Equal("Mine", gateway.Documents["INV-1"]["title"]!.GetValue<string>());
        Assert.Equal(3, gateway.Documents["INV-1"]["version"]!.GetValue<int>());
    }

    [Fact]
    public async Task OverwriteSendsMyChangesAgainstTheServersVersionOnlyOnAnExplicitChoice()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var shell = new FakeShell { Choice = ConflictChoice.Keep };
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);
        form.Draft!.TrySetText("title", "Mine");
        gateway.OtherPersonChanges("INV-1", "Theirs");
        await form.SaveAsync();

        Assert.True(await form.ResolveConflictAsync(ConflictChoice.Overwrite));

        Assert.Null(form.Conflict);
        Assert.Equal("Mine", gateway.Documents["INV-1"]["title"]!.GetValue<string>());
        Assert.Contains("patch INV-1 @2", gateway.Calls);
    }

    [Fact]
    public async Task TheUnsavedChangesGuardListsWhatChangedAndHonoursTheAnswer()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var shell = new FakeShell { Discard = false };
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);

        Assert.True(await form.RequestCloseAsync());
        Assert.DoesNotContain("discard", shell.Asked);

        form.Draft!.TrySetText("title", "Changed");
        Assert.Contains("title", form.UnsavedChanges);
        Assert.False(await form.RequestCloseAsync());
        Assert.Equal(["title"], shell.Discarded);

        shell.Discard = true;
        Assert.True(await form.RequestCloseAsync());

        gateway.Seed("INV-2", title: "Second");
        Assert.True(await form.NavigateAsync("INV-2"));
        Assert.Equal("INV-2", form.DocumentId);
        Assert.Equal("Second", (form.Draft.Get("title") as TextFieldValue)?.Value);
        Assert.False(form.IsDirty);
    }

    [Fact]
    public async Task WithoutTheRightsTheActionsAreDisabledWithAReasonAndTheFormIsReadOnly()
    {
        var gateway = new FakeGateway { Rights = new DocumentRights(true, false, false, false, false, false, false, false) }.Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");

        Assert.True(form.IsReadOnly);
        Assert.All(form.Actions, action => Assert.False(action.IsEnabled));
        Assert.All(form.Actions, action => Assert.False(string.IsNullOrEmpty(action.DisabledReason)));
        Assert.False(await form.SaveAsync() && form.IsDirty);
        Assert.False(await form.RunActionAsync("delete"));
        Assert.DoesNotContain(gateway.Calls, c => c.StartsWith("delete", StringComparison.Ordinal));
    }

    [Fact]
    public async Task APostedDocumentIsReadOnlyAndOffersCancelAndCorrectInsteadOfSaveAndDelete()
    {
        var gateway = new FakeGateway().Seed("INV-1", state: "posted");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");

        Assert.True(form.IsReadOnly);
        Assert.Equal("Submitted", form.StatusBadge);
        var enabled = form.Actions.Where(a => a.IsEnabled).Select(a => a.Id).ToHashSet();
        Assert.Contains("cancel", enabled);
        Assert.Contains("correct", enabled);
        Assert.DoesNotContain("save", enabled);
        Assert.DoesNotContain("submit", enabled);
        Assert.DoesNotContain("delete", enabled);
    }

    [Fact]
    public async Task SubmitConfirmsSavesFirstAndPostsAtTheCurrentVersion()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var shell = new FakeShell { Confirm = false };
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);
        form.Draft!.TrySetText("title", "Ready");

        Assert.False(await form.SubmitAsync());
        Assert.DoesNotContain(gateway.Calls, c => c.StartsWith("Submit", StringComparison.Ordinal));

        shell.Confirm = true;
        Assert.True(await form.SubmitAsync());
        Assert.Contains("Submit INV-1 @2", gateway.Calls);
        Assert.Equal(DocumentStatus.Posted, form.Status);
        Assert.True(form.IsReadOnly);
        Assert.Contains(form.Messages, m => m.Text.Contains("submitted", StringComparison.Ordinal));
    }

    [Fact]
    public async Task CancelAsksForAReasonWhenTheHostSaysTheDocTypeNeedsOne()
    {
        var gateway = new FakeGateway().Seed("INV-1", state: "posted");
        var shell = new FakeShell { Text = "wrong customer" };
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);
        form.RequiresCancelReason = _ => true;

        Assert.True(await form.CancelDocumentAsync());

        Assert.Contains("ask: Cancel Invoice", shell.Asked);
        Assert.Contains("Cancel INV-1 @1 (wrong customer)", gateway.Calls);
        Assert.Equal(DocumentStatus.Cancelled, form.Status);
    }

    [Fact]
    public async Task CorrectOpensTheAmendedDraftInItsOwnTab()
    {
        var gateway = new FakeGateway().Seed("INV-1", state: "posted");
        var shell = new FakeShell();
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);

        Assert.True(await form.CorrectAsync());

        var (doctype, id) = Assert.Single(shell.Opened);
        Assert.Equal("invoice", doctype);
        Assert.StartsWith("AMENDED-", id, StringComparison.Ordinal);
        Assert.Equal(DocumentStatus.Cancelled, form.Status);
    }

    [Fact]
    public async Task DeleteConfirmsAndClosesTheTabAndAReferencedDocumentListsItsReferrers()
    {
        var gateway = new FakeGateway().Seed("INV-1").Seed("INV-2");
        var shell = new FakeShell();
        var first = await Fixtures.OpenAsync(gateway, "INV-1", shell);
        var closed = 0;
        first.CloseRequested += (_, _) => closed++;
        gateway.ReferencedOnDelete = true;

        Assert.False(await first.DeleteAsync());
        Assert.Contains("referrers", shell.Asked);
        Assert.Equal("Payment PAY-7", Assert.Single(shell.Referrers).Reference);
        Assert.Equal(0, closed);
        Assert.True(gateway.Documents.ContainsKey("INV-1"));

        gateway.ReferencedOnDelete = false;
        Assert.True(await first.DeleteAsync());
        Assert.Equal(1, closed);
        Assert.False(gateway.Documents.ContainsKey("INV-1"));
    }

    [Fact]
    public async Task OpeningALinkAsksTheHostToOpenTheTargetDocument()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        gateway.Documents["INV-1"]["customer"] = "CUST-1";
        var shell = new FakeShell();
        var form = await Fixtures.OpenAsync(gateway, "INV-1", shell);

        form.OpenLink("customer");

        Assert.Equal(("party", "CUST-1"), Assert.Single(shell.Opened));
    }

    [Fact]
    public async Task AMissingDocumentFailsTheFormWithTheServersMessage()
    {
        var form = await Fixtures.OpenAsync(new FakeGateway(), "NOPE");

        Assert.Equal(FormPhase.Failed, form.Phase);
        Assert.Equal("There is no such document.", form.LoadError);
    }
}
