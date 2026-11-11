using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 4 (WP-UI-07.T4): the draft decodes the loaded document's <c>_state</c> into
/// a read-only <see cref="DocumentStatus"/> — <c>draft</c>, <c>posted</c>, <c>cancelled</c>,
/// and <see cref="DocumentStatus.Unknown"/> for a value it does not know (the raw text is
/// kept) or for a 0.4.0-style document without <c>_state</c>. It is not a right: the
/// draft neither exposes nor enforces a write permission from it.
/// </summary>
public sealed class DocumentStatusTests
{
    [Theory]
    [InlineData("draft", DocumentStatus.Draft)]
    [InlineData("posted", DocumentStatus.Posted)]
    [InlineData("cancelled", DocumentStatus.Cancelled)]
    [InlineData("Draft", DocumentStatus.Draft)]
    public void KnownStateTextsDecode(string text, DocumentStatus expected)
    {
        var draft = Loaded(text);

        Assert.Equal(expected, draft.Status);
        Assert.Equal(text, draft.RawStatus);
    }

    [Fact]
    public void AnUnknownStateKeepsTheRawText()
    {
        var draft = Loaded("archived");

        Assert.Equal(DocumentStatus.Unknown, draft.Status);
        Assert.Equal("archived", draft.RawStatus);
    }

    [Fact]
    public void ADocumentWithoutStateIsUnknown()
    {
        // A 0.4.0-style body: no _state at all.
        var draft = new DocumentDraft(TestModels.Invoice());
        draft.LoadFrom(TestModels.DocumentJson);

        Assert.Equal(DocumentStatus.Unknown, draft.Status);
        Assert.Null(draft.RawStatus);
    }

    [Fact]
    public void AFreshDraftIsUnknown()
    {
        var draft = new DocumentDraft(TestModels.Invoice());

        Assert.Equal(DocumentStatus.Unknown, draft.Status);
        Assert.Null(draft.RawStatus);
    }

    [Fact]
    public void ReloadingABodyWithoutStateResetsTheStatus()
    {
        var draft = Loaded("posted");
        Assert.Equal(DocumentStatus.Posted, draft.Status);

        draft.LoadFrom(TestModels.DocumentJson);

        Assert.Equal(DocumentStatus.Unknown, draft.Status);
        Assert.Null(draft.RawStatus);
    }

    [Fact]
    public void ANonStringStateIsKeptVerbatimAndUnknown()
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        draft.LoadFrom("""{"id":"INV-0001","_state":{"value":"posted"}}""");

        Assert.Equal(DocumentStatus.Unknown, draft.Status);
        Assert.Equal("""{"value":"posted"}""", draft.RawStatus);
    }

    [Fact]
    public void StatusNeverGatesAMutation()
    {
        // It is not a right: a posted document still accepts edits here, because the host
        // decides what to permit (the permission summary's rights.write).
        var draft = Loaded("posted");

        draft.Table("taxes").Rows[0].SetValue("qty", new DecimalFieldValue(12.5m));

        Assert.True(draft.IsDirty);
        Assert.Equal(DocumentStatus.Posted, draft.Status);
    }

    private static DocumentDraft Loaded(string state)
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        draft.LoadFrom($$"""{"id":"INV-0001","version":3,"_state":"{{state}}","taxes":[{"id":"row-0001","qty":"1.00"}]}""");
        return draft;
    }
}
