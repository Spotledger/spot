using System.Globalization;
using System.Text.Json.Nodes;
using Spotledger.Client.Grid;
using Spotledger.Client.Api;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

public sealed class ListViewModelTests
{
    private static (ListViewModel Model, FakeListGateway Gateway, FakeShell Shell, InMemoryFilterStore Store) Make(
        FakeListGateway? gateway = null, IListExportSink? sink = null, InMemoryFilterStore? store = null)
    {
        gateway ??= new FakeListGateway(1000);
        var shell = new FakeShell();
        store ??= new InMemoryFilterStore();
        return (new ListViewModel(gateway, shell, store, "order", "ana", Fixtures.Options(sink)), gateway, shell, store);
    }

    private static async Task<(ListViewModel Model, FakeListGateway Gateway, FakeShell Shell, InMemoryFilterStore Store)> ReadyAsync(
        FakeListGateway? gateway = null, IListExportSink? sink = null, InMemoryFilterStore? store = null)
    {
        var made = Make(gateway, sink, store);
        await made.Model.LoadAsync(TestContext.Current.CancellationToken);
        await made.Model.Source!.GetWindow(0, 50, TestContext.Current.CancellationToken);
        return made;
    }

    private static JsonNode Last(FakeListGateway gateway) => JsonNode.Parse(gateway.Requests[^1])!;

    // ------------------------------------------------------------------ rights fail closed

    [Fact]
    public async Task WithoutReadRightNothingIsQueried()
    {
        var (model, gateway, _, _) = Make(new FakeListGateway(100, DocumentRights.None));

        await model.LoadAsync(TestContext.Current.CancellationToken);

        Assert.Equal(ListPhase.Denied, model.Phase);
        Assert.Null(model.Source);
        Assert.Empty(gateway.Requests);
        Assert.False(model.CanCreate);
    }

    [Fact]
    public async Task ADocTypeMissingFromThePermissionSummaryIsDenied()
    {
        var gateway = new FakeListGateway(100) { PermissionFields = "{}" };
        var shell = new FakeShell();
        var model = new ListViewModel(new OtherDocTypeGateway(gateway), shell, new InMemoryFilterStore(), "order", "ana", Fixtures.Options());

        await model.LoadAsync(TestContext.Current.CancellationToken);

        Assert.Equal(ListPhase.Denied, model.Phase);
        Assert.Empty(gateway.Requests);
    }

    private sealed class OtherDocTypeGateway(FakeListGateway inner) : IListGateway
    {
        public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken) => inner.GetMetaAsync(doctype, cancellationToken);
        public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
            => Task.FromResult(new PermissionReply(PermissionSnapshot.Decode("""{"data":{"doctypes":[{"doctype":"party","rights":{},"fields":{}}]}}"""), DocumentRights.All));
        public Task<ListPage> QueryAsync(string doctype, string requestJson, CancellationToken cancellationToken) => inner.QueryAsync(doctype, requestJson, cancellationToken);
        public Task<IReadOnlyDictionary<string, string>> ResolveLabelsAsync(string targetDoctype, IReadOnlyCollection<string> ids, CancellationToken cancellationToken) => inner.ResolveLabelsAsync(targetDoctype, ids, cancellationToken);
        public Task DeleteAsync(string doctype, string id, int version, CancellationToken cancellationToken) => inner.DeleteAsync(doctype, id, version, cancellationToken);
        public bool SupportsBulkExport => false;
        public Task<string> ExportCsvAsync(string doctype, IReadOnlyList<string> columns, ListFilter? filter, IReadOnlyList<ListOrder> order, int maxRows, CancellationToken cancellationToken) => inner.ExportCsvAsync(doctype, columns, filter, order, maxRows, cancellationToken);
    }

    [Fact]
    public async Task AMetadataFailureFailsClosedWithTheServersReason()
    {
        var gateway = new FakeListGateway(100) { MetaFailure = new ApiRequestException(Fixtures.Error("api.unavailable", "Metadata is down", 503)) };
        var (model, _, _, _) = Make(gateway);

        await model.LoadAsync(TestContext.Current.CancellationToken);

        Assert.Equal(ListPhase.Failed, model.Phase);
        Assert.Equal("Metadata is down", model.Message);
        Assert.Empty(gateway.Requests);
    }

    [Fact]
    public async Task HiddenFieldsAreNeverColumnsOrRequestedAndNeverOfferedForFilterOrChooser()
    {
        var (model, gateway, _, _) = await ReadyAsync();

        var names = model.Columns!.FieldNames;
        Assert.DoesNotContain("internal_note", names);
        Assert.DoesNotContain("internal_note", model.ChooserFields.Select(f => f.Name));
        Assert.DoesNotContain("internal_note", model.FilterFields.Select(f => f.Name));
        Assert.DoesNotContain("internal_note", gateway.Requests[0], StringComparison.Ordinal);
        Assert.DoesNotContain("items", model.ChooserFields.Select(f => f.Name));
        Assert.False(model.AddCondition(new FilterCondition("internal_note", FilterOperator.Contains, ["x"])));
    }

    [Fact]
    public async Task MaskedValuesShowTheServersMaskAndAreNeverFormattedOrFiltered()
    {
        var (model, _, _, _) = await ReadyAsync();

        var rows = await model.Source!.GetWindow(0, 3, TestContext.Current.CancellationToken);
        var salary = Array.IndexOf(model.Columns!.FieldNames.ToArray(), "salary");

        Assert.Equal("****", rows[0].Cells[salary].Text);
        Assert.Equal("masked", rows[0].Cells[salary].StyleClass);
        Assert.DoesNotContain("salary", model.FilterFields.Select(f => f.Name));
        Assert.DoesNotContain("salary", model.SearchFields.Select(f => f.Name));
    }

    [Fact]
    public void AMaskedCellWithNoTextOfItsOwnStillShowsAMask()
    {
        var model = Fixtures.Model();
        var column = ListColumnPlanner.Build([(model.Field("salary")!, (double?)null)], Fixtures.Permissions().DocTypes["order"])[0];
        var formatter = new ListCellFormatter(CultureInfo.InvariantCulture, TimeZoneInfo.Utc);

        Assert.Equal(ListCellFormatter.MaskText, formatter.Format(column, null, null).Text);
        Assert.Equal(ListCellFormatter.MaskText, formatter.Format(column, System.Text.Json.JsonDocument.Parse("null").RootElement, null).Text);
    }

    [Fact]
    public async Task DefaultColumnsAreTheViewsListFieldsInMetadataOrder()
    {
        var (model, _, _, _) = await ReadyAsync();

        Assert.Equal(["title", "customer", "amount", "status", "salary"], model.Columns!.FieldNames);
        Assert.Equal(["title", "amount"], model.FilterFields.Take(2).Select(f => f.Name));
    }

    // ------------------------------------------------------------------ filters

    [Fact]
    public async Task ChipsBuildTheFilterAndRemovingOneRerunsTheQuery()
    {
        var (model, gateway, _, _) = await ReadyAsync();

        Assert.True(model.AddCondition(new FilterCondition("amount", FilterOperator.From, ["100"])));
        Assert.True(model.AddCondition(new FilterCondition("status", FilterOperator.OneOf, ["Open"])));
        await model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken);
        Assert.Equal("""{"and":[{"field":"amount","op":"ge","value":"100"},{"field":"status","op":"in","value":["Open"]}]}""", Last(gateway)["filter"]!.ToJsonString());
        Assert.Equal("Amount from 100; Status in Open", model.DescribeFilter());
        Assert.Equal("100+ rows", model.CountText());

        model.RemoveCondition(0);
        await model.Source.GetWindow(0, 10, TestContext.Current.CancellationToken);
        Assert.Equal("""{"field":"status","op":"in","value":["Open"]}""", Last(gateway)["filter"]!.ToJsonString());
    }

    [Fact]
    public async Task AnInvalidConditionIsRefusedWithAMessageAndNothingIsSent()
    {
        var (model, gateway, _, _) = await ReadyAsync();
        var before = gateway.Requests.Count;

        var ok = model.AddCondition(new FilterCondition("qty", FilterOperator.Equals, ["abc"]));
        await model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken);

        Assert.False(ok);
        Assert.NotNull(model.Message);
        Assert.Empty(model.Conditions);
        Assert.Equal(before, gateway.Requests.Count);
    }

    [Fact]
    public async Task TooManyFiltersAreRefusedBeforeTheServerWould()
    {
        var (model, _, _, _) = await ReadyAsync();
        var added = 0;
        for (var i = 0; i < 12; i++)
        {
            if (model.AddCondition(new FilterCondition("amount", FilterOperator.Between, ["1", "2"])))
            {
                added++;
            }
        }

        Assert.True(added < 12);
        Assert.Contains("more filters", model.Message, StringComparison.Ordinal);
        Assert.True(ListLimits.Fits(model.Source!.Filter));
    }

    [Fact]
    public async Task QuickSearchLooksInTheShownTextFieldsAsAStructuredOr()
    {
        var (model, gateway, _, _) = await ReadyAsync();

        await model.SetSearchTextAsync("  ana  ");
        await model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken);

        Assert.Equal(["title"], model.SearchFields.Select(f => f.Name));
        Assert.Equal("""{"field":"title","op":"contains","value":"ana","case_insensitive":true}""", Last(gateway)["filter"]!.ToJsonString());

        await model.SetSearchTextAsync(string.Empty);
        await model.Source.GetWindow(0, 10, TestContext.Current.CancellationToken);
        Assert.Null(Last(gateway)["filter"]);
    }

    [Fact]
    public async Task ASearchTypedAgainBeforeTheDebounceEndsNeverRuns()
    {
        var gateway = new FakeListGateway(1000);
        var gate = new TaskCompletionSource();
        var shell = new FakeShell();
        var options = Fixtures.Options() with { Delay = (_, token) => gate.Task.WaitAsync(token) };
        var model = new ListViewModel(gateway, shell, new InMemoryFilterStore(), "order", "ana", options);
        await model.LoadAsync(TestContext.Current.CancellationToken);
        var first = model.SetSearchTextAsync("a");
        var second = model.SetSearchTextAsync("ab");
        gate.SetResult();
        await Task.WhenAll(first, second);
        await model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken);

        Assert.Equal("ab", model.AppliedSearch);
        Assert.Single(gateway.Requests);
        Assert.Contains("\"ab\"", gateway.Requests[0], StringComparison.Ordinal);
    }

    [Fact]
    public async Task SavedFiltersArePerUserPerDocTypeAndReapplyTheSameFilter()
    {
        var store = new InMemoryFilterStore();
        var (model, gateway, _, _) = await ReadyAsync(store: store);
        model.AddCondition(new FilterCondition("status", FilterOperator.OneOf, ["Paid", "Open"]));
        await model.SetSearchTextAsync("inv");
        await model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken);
        var expected = Last(gateway)["filter"]!.ToJsonString();
        Assert.True(await model.PromptSaveFilterAsync(TestContext.Current.CancellationToken));

        var again = new ListViewModel(new FakeListGateway(1000), new FakeShell(), store, "order", "ana", Fixtures.Options());
        await again.LoadAsync(TestContext.Current.CancellationToken);
        var other = new ListViewModel(new FakeListGateway(1000), new FakeShell(), store, "order", "bob", Fixtures.Options());
        await other.LoadAsync(TestContext.Current.CancellationToken);

        Assert.Equal("Mine", Assert.Single(again.SavedFilters).Name);
        Assert.Empty(other.SavedFilters);
        Assert.True(again.ApplySavedFilter("Mine"));
        Assert.Equal(expected, ListQuery.FilterJson(again.Source!.Filter!));
    }

    [Fact]
    public async Task ASavedFilterWhoseFieldIsGoneIsRefusedWholeNotHalfApplied()
    {
        var store = new InMemoryFilterStore();
        await store.SaveAsync("ana", "order", new ListPreferences(null,
            [new SavedFilter("Old", [new FilterCondition("title", FilterOperator.Contains, ["x"]), new FilterCondition("retired_field", FilterOperator.Equals, ["1"])])]), TestContext.Current.CancellationToken);
        var (model, _, _, _) = await ReadyAsync(store: store);

        Assert.False(model.ApplySavedFilter("Old"));
        Assert.Empty(model.Conditions);
        Assert.Null(model.Source!.Filter);
    }

    [Fact]
    public async Task ASavedFilterOnAHiddenOrMaskedFieldIsRefusedSoItCannotRevealThem()
    {
        var store = new InMemoryFilterStore();
        await store.SaveAsync("ana", "order", new ListPreferences(null,
            [new SavedFilter("Peek", [new FilterCondition("internal_note", FilterOperator.Contains, ["x"])]),
             new SavedFilter("Pay", [new FilterCondition("salary", FilterOperator.From, ["100"])])]), TestContext.Current.CancellationToken);
        var (model, _, _, _) = await ReadyAsync(store: store);

        Assert.False(model.ApplySavedFilter("Peek"));
        Assert.False(model.ApplySavedFilter("Pay"));
        Assert.Empty(model.Conditions);
        Assert.Null(model.Source!.Filter);
    }

    [Fact]
    public async Task AMaskedColumnIsNeverSortedOn()
    {
        var (model, _, _, _) = await ReadyAsync();
        var salary = Array.IndexOf(model.Columns!.FieldNames.ToArray(), "salary");

        Assert.False(model.Columns.GridColumns[salary].IsSortable);
        await model.Source!.Sort(salary, GridSortDirection.Ascending);
        Assert.Empty(model.Source.Order);
    }

    // ------------------------------------------------------------------ columns

    [Fact]
    public async Task ColumnChoiceOrderAndWidthPersistPerUserAndComeBack()
    {
        var store = new InMemoryFilterStore();
        var (model, gateway, _, _) = await ReadyAsync(store: store);

        await model.SetColumnsAsync([new ColumnPreference("amount", 150), new ColumnPreference("title"), new ColumnPreference("due")], TestContext.Current.CancellationToken);
        await model.SetColumnWidthAsync(1, 222, TestContext.Current.CancellationToken);
        await model.Source!.GetWindow(0, 5, TestContext.Current.CancellationToken);
        Assert.Equal(["amount", "title", "due"], JsonNode.Parse(gateway.Requests[^1])!["fields"]!.AsArray().Select(f => (string)f!).ToArray());

        var restored = new ListViewModel(new FakeListGateway(1000), new FakeShell(), store, "order", "ana", Fixtures.Options());
        await restored.LoadAsync(TestContext.Current.CancellationToken);
        var stranger = new ListViewModel(new FakeListGateway(1000), new FakeShell(), store, "order", "bob", Fixtures.Options());
        await stranger.LoadAsync(TestContext.Current.CancellationToken);

        Assert.Equal(["amount", "title", "due"], restored.Columns!.FieldNames);
        Assert.Equal(150, restored.CurrentLayout[0].Width);
        Assert.Equal(222, restored.CurrentLayout[1].Width);
        Assert.Equal(["title", "customer", "amount", "status", "salary"], stranger.Columns!.FieldNames);
    }

    [Fact]
    public async Task StoredColumnsThatAreHiddenOrUnknownAreDroppedAndAtLeastTheDefaultsRemain()
    {
        var store = new InMemoryFilterStore();
        await store.SaveAsync("ana", "order", new ListPreferences([new ColumnPreference("internal_note"), new ColumnPreference("ghost")], []), TestContext.Current.CancellationToken);
        var (model, _, _, _) = await ReadyAsync(store: store);

        Assert.DoesNotContain("internal_note", model.Columns!.FieldNames);
        Assert.NotEmpty(model.Columns.FieldNames);
    }

    [Fact]
    public async Task ReorderingKeepsTheCellsInStepWithTheGridsModel()
    {
        var (model, _, _, _) = await ReadyAsync();

        await model.MoveColumnAsync(0, 2, TestContext.Current.CancellationToken);
        var rows = await model.Source!.GetWindow(0, 2, TestContext.Current.CancellationToken);

        Assert.Equal(["customer", "amount", "title", "status", "salary"], model.Columns!.FieldNames);
        Assert.Equal("Order 0", rows[0].Cells[2].Text);
    }

    [Fact]
    public async Task ResettingColumnsRestoresTheViewsDefaults()
    {
        var (model, _, _, _) = await ReadyAsync();
        await model.SetColumnsAsync([new ColumnPreference("due")], TestContext.Current.CancellationToken);

        await model.ResetColumnsAsync(TestContext.Current.CancellationToken);

        Assert.Equal(["title", "customer", "amount", "status", "salary"], model.Columns!.FieldNames);
    }

    // ------------------------------------------------------------------ actions

    [Fact]
    public async Task OpenAndNewGoThroughTheFormsShell()
    {
        var (model, _, shell, _) = await ReadyAsync();

        model.Open("d-000004");
        Assert.True(model.New());

        Assert.Equal([("order", (string?)"d-000004"), ("order", null)], shell.Opened);
    }

    [Fact]
    public async Task NewNeedsTheCreateRightFromBothTheRightsAndThePermissionSummary()
    {
        var (a, _, shellA, _) = await ReadyAsync(new FakeListGateway(10, new DocumentRights(true, false, false, false, false, false, false, false)));
        var (b, _, shellB, _) = await ReadyAsync(new FakeListGateway(10) { CanCreate = false });

        Assert.False(a.New());
        Assert.False(b.New());
        Assert.Empty(shellA.Opened);
        Assert.Empty(shellB.Opened);
    }

    [Fact]
    public async Task SelectingOneRowRaisesTheSplitEventAndSeveralOrNoneRaiseNull()
    {
        var (model, _, _, _) = await ReadyAsync();
        var seen = new List<ListRowSelection>();
        model.SelectedRowChanged += (_, e) => seen.Add(e);

        model.SetSelection(["d-000001"]);
        model.SetSelection(["d-000001", "d-000002"]);
        model.SetSelection([]);

        Assert.Equal([new ListRowSelection("order", "d-000001"), new ListRowSelection("order", null), new ListRowSelection("order", null)], seen);
    }

    [Fact]
    public async Task BulkDeleteSendsEachDeleteAtItsLoadedVersionAndRefreshes()
    {
        var (model, gateway, shell, _) = await ReadyAsync();
        model.SetSelection(["d-000002", "d-000005"]);
        var before = gateway.Requests.Count;

        var summary = await model.DeleteSelectedAsync(TestContext.Current.CancellationToken);
        await model.Source!.GetWindow(0, 5, TestContext.Current.CancellationToken);

        Assert.Equal((2, 2, 0), (summary!.Attempted, summary.Deleted, summary.Failures.Count));
        Assert.Equal([("d-000002", 3), ("d-000005", 6)], gateway.Deleted);
        Assert.Contains("Delete 2 documents", shell.Asked[0], StringComparison.Ordinal);
        Assert.True(gateway.Requests.Count > before);
        Assert.Equal("Deleted 2 documents.", model.Message);
    }

    [Fact]
    public async Task BulkDeleteListsWhatCouldNotGoWithTheDocumentsThatStillRefer()
    {
        var (model, gateway, shell, _) = await ReadyAsync();
        gateway.DeleteErrors["d-000002"] = Fixtures.Error("document.linked", "Linked", 409,
            new ApiFieldError("invoice INV-7", "document.linked", "Invoice refers to it"), new ApiFieldError("payment PAY-2", "document.linked", "Payment refers to it"));
        gateway.DeleteErrors["d-000003"] = Fixtures.Error("document.not_draft", "Only drafts can be deleted", 409);
        model.SetSelection(["d-000001", "d-000002", "d-000003"]);

        var summary = await model.DeleteSelectedAsync(TestContext.Current.CancellationToken);

        Assert.Equal(3, summary!.Attempted);
        Assert.Equal(1, summary.Deleted);
        Assert.Equal(["d-000002", "d-000003"], summary.Failures.Select(f => f.Id));
        Assert.Equal("Deleted 1 of 3. 2 could not be deleted.", model.Message);
        Assert.Contains("referrers: Cannot delete: 2 not deleted", shell.Asked);
        Assert.Equal(
            ["Order 2: invoice INV-7 | Invoice refers to it", "Order 2: payment PAY-2 | Payment refers to it", "Order 3 | Only drafts can be deleted"],
            shell.Referrers.Select(r => r.Reference + (r.Detail is null ? string.Empty : " | " + r.Detail)));
    }

    [Fact]
    public async Task DeclinedOrUnrightedDeleteSendsNothing()
    {
        var (model, gateway, shell, _) = await ReadyAsync();
        model.SetSelection(["d-000002"]);
        shell.Confirm = false;
        Assert.Null(await model.DeleteSelectedAsync(TestContext.Current.CancellationToken));

        var (noRight, noRightGateway, _, _) = await ReadyAsync(new FakeListGateway(10, new DocumentRights(true, true, true, false, false, false, false, false)));
        noRight.SetSelection(["d-000002"]);

        Assert.False(noRight.CanDelete);
        Assert.Null(await noRight.DeleteSelectedAsync(TestContext.Current.CancellationToken));
        Assert.Empty(gateway.Deleted);
        Assert.Empty(noRightGateway.Deleted);
    }

    [Fact]
    public async Task ExportWithoutBulkEndpointUsesTheLoadedRowsAndSaysSo()
    {
        var sink = new CapturingSink();
        var gateway = new FakeListGateway(1000) { Prefix = "=" };
        var (model, _, _, _) = await ReadyAsync(gateway, sink);

        var rows = await model.ExportAsync(TestContext.Current.CancellationToken);

        Assert.Equal(100, rows);
        var csv = Assert.Single(sink.Saved).Csv;
        Assert.StartsWith("Title,Customer,Amount,Status,Salary\r\n", csv, StringComparison.Ordinal);
        Assert.Contains("'=Order 0,Party p-0,5.50,Open,****", csv, StringComparison.Ordinal);
        Assert.Contains("only loaded rows are exported", model.Message, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ExportThroughTheBulkEndpointWhenTheGatewayOffersIt()
    {
        var sink = new CapturingSink();
        var gateway = new FakeListGateway(1000) { BulkExport = true, ExportCsv = "id,title\r\nd-1,A\r\nd-2,B\r\n" };
        var (model, _, _, _) = await ReadyAsync(gateway, sink);

        var rows = await model.ExportAsync(TestContext.Current.CancellationToken);

        Assert.Equal(2, rows);
        Assert.Equal(gateway.ExportCsv, Assert.Single(sink.Saved).Csv);
    }

    [Fact]
    public async Task AQueryFailureShowsTheServersMessageNotAStackTrace()
    {
        var gateway = new FakeListGateway(1000);
        var (model, _, _, _) = Make(gateway);
        await model.LoadAsync(TestContext.Current.CancellationToken);
        gateway.FailNext = new ApiRequestException(Fixtures.Error("api.unavailable", "The server is busy", 503));

        await Assert.ThrowsAsync<ApiRequestException>(() => model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken));

        Assert.Equal("The server is busy", model.Message);
        Assert.Contains("The server is busy", model.StatusText, StringComparison.Ordinal);
    }

    [Fact]
    public void TabRequestsAreKeyedPerDocTypeAndValidated()
    {
        var request = ListTabs.Open("order", "Orders");

        Assert.Equal("list:order", request.Key);
        Assert.Equal("order", ListTabs.ParseRequest(request));
        Assert.Throws<ArgumentException>(() => ListTabs.ParseRequest(request with { Arguments = new Dictionary<string, string> { ["doctype"] = "Order;drop" } }));
        var kinds = new Spotledger.Client.Shell.WorkspaceTabKinds();
        ListTabs.Register(kinds, doctype => new ListViewModel(new FakeListGateway(1), new FakeShell(), new InMemoryFilterStore(), doctype, "ana"));
        Assert.IsType<ListViewModel>(kinds.Open(request));
        Assert.Same(kinds.Open(request), kinds.Open(request));
    }
}
