using Spotledger.Client.Expressions;
using Spotledger.Client.Extensions;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>The live slot view and its actions: visibility, enabled state, permission
/// filtering, the busy rule and the dispatcher contract.</summary>
public sealed class SlotViewTests
{
    [Fact]
    public void TheTaskToolbarShowsBothActionsForAPermittedUser()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        Assert.Equal(["sample.start_work", "sample.open_time"], view.Entries.Select(entry => entry.Id));
        ActionModel start = Assert.IsType<ActionModel>(view.Entries[0]);
        Assert.True(start.IsVisible);
        Assert.True(start.IsEnabled);
        Assert.Equal("Start work", start.Label);
        Assert.Equal("play", start.Icon);
        Assert.Equal("Start work on this task?", start.ConfirmText);
        Assert.Equal(["Start work", "Time entries"], view.Entries.Cast<ActionModel>().Select(action => action.Label));
    }

    [Fact]
    public void AnActionTheUserMayNotSeeIsHiddenByItsPermission()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext(permissions: ["time_entry:read"]);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        Assert.Equal(["sample.open_time"], view.Entries.Select(entry => entry.Id));
        Assert.False(view.AllEntries.Single(entry => entry.Id == "sample.start_work").IsVisible);
    }

    [Fact]
    public void EnabledIfFalseDisablesWithoutHiding()
    {
        DocumentDraft draft = Sample.TaskDraft();
        draft.Set("assignee", new LinkFieldValue(null, null));
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext(draft);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        ActionModel start = Assert.IsType<ActionModel>(view.Entries[0]);
        Assert.True(start.IsVisible);
        Assert.False(start.IsEnabled);
    }

    [Fact]
    public void VisibleIfFalseHidesTheAction()
    {
        DocumentDraft draft = Sample.TaskDraft();
        draft.Set("status", new TextFieldValue("Closed"));
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext(draft);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        Assert.Equal(["sample.open_time"], view.Entries.Select(entry => entry.Id));
    }

    [Fact]
    public void EditingTheDraftReevaluatesTheSlotWithoutARebuild()
    {
        DocumentDraft draft = Sample.TaskDraft();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext(draft);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        int changes = 0;
        view.Changed += (_, _) => changes++;
        Assert.Equal(2, view.Entries.Count);
        ActionModel start = (ActionModel)view.Entries[0];
        draft.Set("status", new TextFieldValue("Closed"));
        Assert.Single(view.Entries);
        Assert.False(start.IsVisible);
        Assert.True(changes > 0);
    }

    [Fact]
    public void ASlotForOneDoctypeDoesNotShowAnotherDoctypesItems()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView task = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        using SlotView project = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "project");
        Assert.Equal(2, task.Entries.Count);
        Assert.Empty(project.Entries);
    }
}

/// <summary>One click, one request: the dispatcher contract, the busy rule, confirmation and
/// the way an answer is applied.</summary>
public sealed class ActionTests
{
    [Fact]
    public async Task TheCallActionSendsItsMethodAndArgumentsExactlyOnce()
    {
        FakeDispatcher dispatcher = new();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ActionModel start = (ActionModel)view.Entries[0];
        await start.InvokeAsync(TestContext.Current.CancellationToken);
        ActionRequest request = Assert.Single(dispatcher.Requests);
        Assert.Equal(ActionEffectKind.Call, request.Kind);
        Assert.Equal("task", request.Doctype);
        Assert.Equal("start_work", request.Method);
        KeyValuePair<string, ExpressionValue> argument = Assert.Single(request.Arguments!);
        Assert.Equal("task", argument.Key);
        Assert.Equal("TASK-1", Assert.IsType<TextValue>(argument.Value).Value);
        Assert.Equal("TASK-1", request.DocumentId);
        Assert.Equal(4, request.DocumentVersion);
        Assert.False(string.IsNullOrEmpty(request.IdempotencyKey));
        Assert.Contains($"confirm:Start work on this task?", shell.Calls);
        Assert.Contains("refresh:task:TASK-1", shell.Calls);
        Assert.False(start.IsBusy);
    }

    [Fact]
    public async Task ABusyActionIgnoresASecondClick()
    {
        FakeDispatcher dispatcher = new() { Hold = new TaskCompletionSource() };
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ActionModel start = (ActionModel)view.Entries[0];
        Task first = start.InvokeAsync(TestContext.Current.CancellationToken);
        Assert.True(start.IsBusy);
        Assert.False(start.IsEnabled);
        await start.InvokeAsync(TestContext.Current.CancellationToken);
        dispatcher.Hold.SetResult();
        await first;
        Assert.Single(dispatcher.Requests);
        Assert.False(start.IsBusy);
    }

    [Fact]
    public async Task CancellingTheConfirmationSendsNothing()
    {
        FakeDispatcher dispatcher = new();
        FakeShell shell = new() { ConfirmAnswer = false };
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        await ((ActionModel)view.Entries[0]).InvokeAsync(TestContext.Current.CancellationToken);
        Assert.Empty(dispatcher.Requests);
    }

    [Fact]
    public async Task AFailedAnswerShowsTheProblemOnceAndReportsItOnce()
    {
        FakeDispatcher dispatcher = new();
        dispatcher.Outcomes.Enqueue(ActionOutcome.Failure("task.start_work: the task is already started"));
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ActionModel start = (ActionModel)view.Entries[0];
        await start.InvokeAsync(TestContext.Current.CancellationToken);
        Assert.Equal(["confirm:Start work on this task?", "message:task.start_work: the task is already started"], shell.Calls);
        ExtensionDiagnostic diagnostic = Assert.Single(set.Diagnostics, item => item.Code == ExtensionCodes.DispatchFailed);
        Assert.Contains("already started", diagnostic.Message);
    }

    [Fact]
    public async Task ADispatcherThatThrowsBecomesAMessageNotAnException()
    {
        FakeDispatcher dispatcher = new() { Throw = new InvalidOperationException("network down") };
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        await ((ActionModel)view.Entries[0]).InvokeAsync(TestContext.Current.CancellationToken);
        Assert.Contains("message:network down", shell.Calls);
    }

    [Fact]
    public async Task TheOpenActionNavigatesWithEvaluatedFilters()
    {
        FakeDispatcher dispatcher = new();
        FakeShell shell = new();
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        ActionModel open = (ActionModel)view.Entries[1];
        await open.InvokeAsync(TestContext.Current.CancellationToken);
        ActionRequest request = Assert.Single(dispatcher.Requests);
        Assert.Equal(ActionEffectKind.Open, request.Kind);
        Assert.Equal("sample.time_entries", request.Destination);
        KeyValuePair<string, ExpressionValue> filter = Assert.Single(request.Filters!);
        Assert.Equal("TASK-1", Assert.IsType<TextValue>(filter.Value).Value);
    }

    [Fact]
    public async Task AListScopedCallSendsTheSelectedIds()
    {
        const string document = """
        {
          "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample",
          "actions": [
            { "id": "sample.bulk", "origin": "app:sample", "label": "Bulk", "slot": "list.toolbar", "doctype": "task",
              "call": { "doctype": "task", "method": "start_work", "args": { "note": "'all'" } } }
          ]
        }
        """;
        FakeDispatcher dispatcher = new();
        FakeShell shell = new();
        ExtensionSet set = ExtensionSet.Parse(document, Sample.Facts);
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.ListToolbar, context, "task", new ExtensionServices(dispatcher, shell));
        await ((ActionModel)view.Entries[0]).InvokeAsync(TestContext.Current.CancellationToken);
        ActionRequest request = Assert.Single(dispatcher.Requests);
        Assert.Equal(2, request.Arguments!.Count);
        Assert.Equal("ids", request.Arguments[1].Key);
        Assert.Equal(2, Assert.IsType<ListValue>(request.Arguments[1].Value).Values.Count);
        // The call targets the DocType of the open document, so it carries the If-Match version.
        Assert.Equal(4, request.DocumentVersion);
    }

    [Fact]
    public async Task ARenderedButUnwiredActionReportsThatTheHostWiredNothing()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.TaskContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        await ((ActionModel)view.Entries[1]).InvokeAsync(TestContext.Current.CancellationToken);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.DispatchFailed);
    }
}
