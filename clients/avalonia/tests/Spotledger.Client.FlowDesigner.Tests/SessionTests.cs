using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The session actions: save as draft sends the canonical text and adopts the version the server
/// answers with, the publish action only asks for review, the version list is read-only, and a
/// reader is refused everything — with a reason a person can act on, and without a request going
/// out.
public sealed class SessionTests
{

    private static (FlowDesignerSession Session, FakeServer Server) Make(FlowPermissions? permissions = null)
    {
        FakeServer server = new();
        FlowDesignerSession session = new(
            server,
            FlowFixtures.Document(),
            permissions ?? FlowPermissions.Editor,
            new FlowStrings(RunLabels.Resolve("Application", "Applications")));
        return (session, server);
    }

    [Fact]

    public async Task SavingADraftSendsTheCanonicalTextAndAdoptsTheServersVersion()
    {
        (FlowDesignerSession session, FakeServer server) = Make();
        session.Editor.Execute(new SetTitleCommand("Order to Cash, revised"));

        FlowActionResult result = await session.SaveDraftAsync(CancellationToken.None);

        Assert.True(result.Ok);
        Assert.Equal("Draft saved as version 7.", result.Message);
        Assert.Contains("title = \"Order to Cash, revised\"", Assert.Single(server.Saved), StringComparison.Ordinal);
        Assert.Contains("version = 1", server.Saved[0], StringComparison.Ordinal);
        Assert.Equal(7, session.Document.Version);
        Assert.Contains("version = 7", session.Document.ToText(), StringComparison.Ordinal);
        Assert.False(session.Editor.CanUndo);
    }

    [Fact]

    public async Task ASecondSaveCarriesTheHashTheFirstOneReturned()
    {
        (FlowDesignerSession session, FakeServer server) = Make();
        await session.SaveDraftAsync(CancellationToken.None);
        Assert.Null(server.ExpectedHash);

        await session.SaveDraftAsync(CancellationToken.None);

        Assert.Equal("hash-7", server.ExpectedHash);
        Assert.Equal(2, server.Saved.Count);
    }

    [Fact]

    public async Task AskingForPublicationSendsTheTextAndReportsTheReview()
    {
        (FlowDesignerSession session, FakeServer server) = Make();

        FlowActionResult result = await session.RequestPublishAsync(CancellationToken.None);

        Assert.True(result.Ok);
        Assert.Equal(FlowStrings.PublishRequested, result.Message);
        Assert.Single(server.Published);
        Assert.Contains("name = \"order_to_cash\"", server.Published[0], StringComparison.Ordinal);
    }

    [Fact]

    public async Task TheVersionListIsReadOnlyData()
    {
        (FlowDesignerSession session, FakeServer server) = Make();
        server.VersionRows.Add(new FlowVersionSummary(3, FlowVersionStatus.Published, "asha", new DateTimeOffset(2026, 10, 2, 9, 0, 0, TimeSpan.Zero), "abc"));
        server.VersionRows.Add(new FlowVersionSummary(4, FlowVersionStatus.Draft, "ravi", new DateTimeOffset(2026, 10, 5, 11, 30, 0, TimeSpan.Zero), "def"));

        IReadOnlyList<FlowVersionSummary> versions = await session.LoadVersionsAsync(CancellationToken.None);

        Assert.Equal(2, versions.Count);
        Assert.Equal(FlowVersionStatus.Published, versions[0].Status);
        Assert.Equal("ravi", versions[1].Author);
        Assert.Equal(versions.Count, session.Versions.Count);
    }

    [Fact]

    public async Task AReaderIsRefusedASaveAndNothingIsSent()
    {
        (FlowDesignerSession session, FakeServer server) = Make(FlowPermissions.Reader);

        FlowActionResult save = await session.SaveDraftAsync(CancellationToken.None);
        FlowActionResult publish = await session.RequestPublishAsync(CancellationToken.None);

        Assert.False(save.Ok);
        Assert.Equal(FlowStrings.RefusedReadOnly, save.Error);
        Assert.False(publish.Ok);
        Assert.False(session.Editor.CanEdit);
        Assert.True(session.IsReadOnly);
        Assert.Empty(server.Saved);
        Assert.Empty(server.Published);
    }

    [Fact]

    public async Task AnEditorWithoutThePublishRightIsToldWhy()
    {
        (FlowDesignerSession session, FakeServer server) = Make(new FlowPermissions(true, true, false));

        FlowActionResult publish = await session.RequestPublishAsync(CancellationToken.None);

        Assert.False(publish.Ok);
        Assert.Equal(FlowStrings.PublishDenied, publish.Error);
        Assert.Empty(server.Published);
    }

    [Fact]

    public async Task TheStringsCarryTheOrganisationsRunLabel()
    {
        (FlowDesignerSession session, _) = Make();

        Assert.Contains("Application", session.Strings.RunLabelPreview, StringComparison.Ordinal);
        Assert.DoesNotContain("Request", session.Strings.RunLabelPreview, StringComparison.Ordinal);
        Assert.NotNull(await session.LoadKindsAsync(CancellationToken.None));
        Assert.Equal(8, session.Catalogue.Kinds.Count);
    }

    [Fact]

    public void ARefusalCanBeReportedByTheHostWithoutLosingTheDocument()
    {
        (FlowDesignerSession session, _) = Make();

        session.ReportError(FlowStrings.DraftConflict);

        Assert.Equal(FlowStrings.DraftConflict, session.ErrorMessage);
        Assert.NotNull(session.Document.Node("start"));
    }
}
