using System.Text.Json;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 5's other half: the draft supplies the lookup request's <c>doc</c> and
/// <c>parent</c> snapshots in the value encoding, and nothing else. The client evaluates
/// no filter — the server applies the field's filter over exactly these two maps
/// (ADR-068 §3, amendment A3.2) — so the tests here assert the snapshots' shape and that
/// no filter-related value is ever added.
/// </summary>
public sealed class LookupSnapshotTests
{
    [Fact]
    public void TheDocumentSnapshotCarriesEveryScalarInFieldOrder()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        draft.Set("amount", new DecimalFieldValue(ExactDecimal.FromCanonical("10.50")));
        draft.Set("has_receipt", new BoolFieldValue(true));
        draft.Set("kind", new SelectFieldValue("Travel"));

        var snapshot = draft.LookupSnapshot();

        Assert.Null(snapshot.Parent);
        using var document = JsonDocument.Parse(snapshot.Doc);
        var root = document.RootElement;
        Assert.Equal("10.50", root.GetProperty("amount").GetString());
        Assert.True(root.GetProperty("has_receipt").GetBoolean());
        Assert.Equal("Travel", root.GetProperty("kind").GetString());
        Assert.Equal("", root.GetProperty("receipt_note").GetString());
        // A child table is not a lookup value.
        Assert.False(root.TryGetProperty("items", out _));
        // No filter, no seam, no extra: the request stays closed and the server has the
        // field's filter.
        Assert.Equal(10, root.EnumerateObject().Count());
    }

    [Fact]
    public void ARowSnapshotCarriesTheRowAsDocAndTheDocumentAsParent()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        draft.Set("amount", new DecimalFieldValue(ExactDecimal.FromCanonical("99.00")));
        var row = draft.Table("items").AddRow();
        row.SetValue("qty", new DecimalFieldValue(ExactDecimal.FromCanonical("2.00")));

        var snapshot = draft.LookupSnapshot("items", 1);

        using var doc = JsonDocument.Parse(snapshot.Doc);
        Assert.Equal("2.00", doc.RootElement.GetProperty("qty").GetString());
        Assert.Equal("each", doc.RootElement.GetProperty("unit").GetString());
        Assert.False(doc.RootElement.TryGetProperty("id", out _));
        Assert.False(doc.RootElement.TryGetProperty("idx", out _));

        Assert.NotNull(snapshot.Parent);
        using var parent = JsonDocument.Parse(snapshot.Parent!);
        Assert.Equal("99.00", parent.RootElement.GetProperty("amount").GetString());
        Assert.False(parent.RootElement.TryGetProperty("items", out _));
    }

    [Fact]
    public void TheSnapshotFollowsTheDraftAsItChanges()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        draft.Set("note", new TextFieldValue("first"));
        Assert.Contains("\"first\"", draft.LookupSnapshot().Doc, StringComparison.Ordinal);

        draft.Set("note", new TextFieldValue("second"));
        Assert.Contains("\"second\"", draft.LookupSnapshot().Doc, StringComparison.Ordinal);
        Assert.DoesNotContain("first", draft.LookupSnapshot().Doc, StringComparison.Ordinal);
    }

    [Fact]
    public void AnOutOfRangeRowIsRefused()
    {
        var draft = new DocumentDraft(RuleTestModels.Claim());
        Assert.Throws<ArgumentOutOfRangeException>(() => draft.LookupSnapshot("items", 1));
    }
}
