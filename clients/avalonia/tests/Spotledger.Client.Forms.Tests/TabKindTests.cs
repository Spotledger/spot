using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

public sealed class TabKindTests
{
    private static (WorkspaceTabKinds Kinds, List<DocumentFormViewModel> Created) Registry()
    {
        var kinds = new WorkspaceTabKinds();
        var created = new List<DocumentFormViewModel>();
        DocumentTabs.Register(kinds, (doctype, id) =>
        {
            var form = new DocumentFormViewModel(new FakeGateway(), doctype, id);
            created.Add(form);
            return form;
        });
        return (kinds, created);
    }

    [Fact]
    public void ADocumentOpensOnceAndAskingAgainSelectsTheOpenTab()
    {
        var (kinds, created) = Registry();

        var first = kinds.Open(DocumentTabs.Open("invoice", "INV-1"));
        var again = kinds.Open(DocumentTabs.Open("invoice", "INV-1"));
        var other = kinds.Open(DocumentTabs.Open("invoice", "INV-2"));

        Assert.Same(first, again);
        Assert.NotSame(first, other);
        Assert.Equal(2, created.Count);
        Assert.Equal("document:invoice:INV-1", DocumentTabs.KeyFor("invoice", "INV-1"));
    }

    [Fact]
    public void EveryNewDocumentGetsItsOwnTabAndAClosedTabCanOpenAgain()
    {
        var (kinds, created) = Registry();

        var a = kinds.Open(DocumentTabs.New("invoice"));
        var b = kinds.Open(DocumentTabs.New("invoice"));
        Assert.NotSame(a, b);

        var open = kinds.Open(DocumentTabs.Open("invoice", "INV-1"));
        Assert.True(kinds.Close(DocumentTabs.KeyFor("invoice", "INV-1")));
        Assert.NotSame(open, kinds.Open(DocumentTabs.Open("invoice", "INV-1")));
        Assert.Equal(4, created.Count);
    }

    [Fact]
    public void ARegisteredKindIsRefusedASecondTime()
    {
        var (kinds, _) = Registry();

        Assert.True(kinds.IsRegistered(DocumentTabs.Kind));
        Assert.Throws<InvalidOperationException>(() => DocumentTabs.Register(kinds, (_, _) => throw new NotSupportedException()));
        Assert.Throws<InvalidOperationException>(() => kinds.Open(new WorkspaceTabRequest("unknown", "k", "t", new Dictionary<string, string>())));
    }
}

public sealed class DocumentCommandTests
{
    [Fact]
    public void NewAndOpenCommandsNameTheirDocTypeAndResolveToTabRequests()
    {
        var create = DocumentCommands.New("sales_invoice", "Sales Invoice");
        var open = DocumentCommands.Open("sales_invoice", "Sales Invoice");

        Assert.Equal("document.new.sales_invoice", create.Id);
        Assert.Equal("Create", create.Category);
        Assert.Equal("Open", open.Category);
        Assert.True(DocumentCommands.TryParse(create.Id, out var isNew, out var doctype));
        Assert.True(isNew);
        Assert.Equal("sales_invoice", doctype);
        Assert.Equal(DocumentTabs.New("sales_invoice").Key, DocumentCommands.RequestFor(create.Id)!.Key);
        Assert.Equal(DocumentTabs.Open("sales_invoice", "SI-9").Key, DocumentCommands.RequestFor(open.Id, "SI-9")!.Key);
        Assert.Null(DocumentCommands.RequestFor(open.Id));
        Assert.False(DocumentCommands.TryParse("file.quit", out _, out _));
    }

    [Fact]
    public async Task ABoundTabFollowsTheFormsTitleAndUnsavedMarker()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = await Fixtures.OpenAsync(gateway, "INV-1");
        var tab = new WorkspaceTabViewModel("Selling", "loading");
        using var binding = DocumentTabs.Bind(tab, form);

        Assert.Equal("Invoice First invoice", tab.Title);
        Assert.False(tab.IsDirty);
        form.Draft!.TrySetText("title", "Edited");
        Assert.True(tab.IsDirty);
        Assert.Equal("Invoice Edited", tab.Title);
    }
}
