using System.Text.Json;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;
using Spotledger.Client.Api;
using Spotledger.Client.ChildTable;
using Spotledger.Client.Forms;
using Spotledger.Client.Lists;
using Spotledger.Client.Metadata;
using Spotledger.Client.Workspace.Keyboard;
using Spotledger.Client.Workspace.Split;

namespace Spotledger.Client.Workspace.Tests.Split;

/// <summary>One row of the task list the workflows work.</summary>
public sealed record TaskRow(string Id, string Title, bool Done);

/// <summary>
/// The DocType the workflows use: a task with a title, a done flag and a line table. It is the shape of
/// the sample app's Tasks (title, status, child rows) without borrowing its metadata.
/// </summary>
public static class TaskModel
{
    public static DocTypeModel Lines() => new("taskline", "Line", 1, false,
    [
        new FieldModel("item", "Item", FieldKind.Text),
        new FieldModel("qty", "Quantity", FieldKind.DecimalNumber, scale: 2, precision: 18),
    ]);

    public static DocTypeModel Task() => new("task", "Task", 1, false,
    [
        new FieldModel("title", "Title", FieldKind.Text, required: true, hints: new FieldHintsModel(InListView: true, InStandardFilter: true)),
        new FieldModel("done", "Done", FieldKind.Bool, hints: new FieldHintsModel(InListView: true)),
        new FieldModel("items", "Lines", FieldKind.Table, childDoctype: "taskline"),
    ], new Dictionary<string, DocTypeModel> { ["items"] = Lines() });

    public static string Document(TaskRow row, int version = 2, bool withLines = true)
        => string.Format(
            System.Globalization.CultureInfo.InvariantCulture,
            "{{\"id\":\"{0}\",\"version\":{1},\"title\":\"{2}\",\"done\":{3},\"items\":{4}}}",
            row.Id,
            version,
            row.Title.Replace("\"", "\\\"", StringComparison.Ordinal),
            row.Done ? "true" : "false",
            withLines ? "[{\"id\":\"r1\",\"idx\":1,\"item\":\"Bolt\",\"qty\":\"2.00\"}]" : "[]");
}

/// <summary>A server fake for the list: fixed rows, substring search, opaque cursor paging.</summary>
public sealed class FakeTaskListGateway : IListGateway
{
    public List<TaskRow> Rows { get; } = [];
    public List<string> Requests { get; } = [];
    public int PageSize { get; set; } = 100;

    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken)
        => Task.FromResult(TaskModel.Task());

    public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
        => Task.FromResult(new PermissionReply(
            PermissionSnapshot.Decode("""{"etag":"e1","data":{"doctypes":[{"doctype":"task","rights":{"create":true}}]}}"""),
            DocumentRights.All));

    public Task<ListPage> QueryAsync(string doctype, string requestJson, CancellationToken cancellationToken)
    {
        Requests.Add(requestJson);
        using var document = JsonDocument.Parse(requestJson);
        var root = document.RootElement;
        var fields = root.GetProperty("fields").EnumerateArray().Select(field => field.GetString()!).ToArray();
        var limit = root.TryGetProperty("limit", out var limitValue) ? limitValue.GetInt32() : PageSize;
        var offset = root.TryGetProperty("cursor", out var cursor) && cursor.GetString() is { } text
            ? int.Parse(text[2..], System.Globalization.CultureInfo.InvariantCulture)
            : 0;
        var matching = Rows.AsEnumerable();
        // The quick search reaches the server as a "contains" condition over the searchable fields, so the
        // fake narrows on every text value the filter carries.
        foreach (var needle in TextValues(root))
        {
            matching = matching.Where(row =>
                row.Title.Contains(needle, StringComparison.OrdinalIgnoreCase)
                || (row.Done ? "Done" : "Open").Contains(needle, StringComparison.OrdinalIgnoreCase));
        }

        var all = matching.ToList();
        var records = all.Skip(offset).Take(limit).Select(row => Record(row, fields)).ToList();
        var next = offset + limit < all.Count
            ? "c:" + (offset + limit).ToString(System.Globalization.CultureInfo.InvariantCulture)
            : null;
        return Task.FromResult(new ListPage(records, next, all.Count));
    }

    public Task<IReadOnlyDictionary<string, string>> ResolveLabelsAsync(string targetDoctype, IReadOnlyCollection<string> ids, CancellationToken cancellationToken)
        => Task.FromResult<IReadOnlyDictionary<string, string>>(ids.ToDictionary(id => id, id => id, StringComparer.Ordinal));

    public Task DeleteAsync(string doctype, string id, int version, CancellationToken cancellationToken)
    {
        _ = Rows.RemoveAll(row => row.Id == id);
        return Task.CompletedTask;
    }

    public bool SupportsBulkExport => false;

    public Task<string> ExportCsvAsync(string doctype, IReadOnlyList<string> columns, ListFilter? filter, IReadOnlyList<ListOrder> order, int maxRows, CancellationToken cancellationToken)
        => throw new NotSupportedException();

    /// <summary>The text the quick search put in the filter (the condition's values, not its field names).</summary>
    private static List<string> TextValues(JsonElement root)
        => root.TryGetProperty("filter", out var filter) ? Walks(filter).ToList() : [];

    private static IEnumerable<string> Walks(JsonElement element)
    {
        if (element.ValueKind == JsonValueKind.Object)
        {
            foreach (var property in element.EnumerateObject())
            {
                if (property.Name is "value" or "values")
                {
                    foreach (var value in Leaves(property.Value))
                    {
                        yield return value;
                    }
                }
                else
                {
                    foreach (var nested in Walks(property.Value))
                    {
                        yield return nested;
                    }
                }
            }
        }
        else if (element.ValueKind == JsonValueKind.Array)
        {
            foreach (var nested in element.EnumerateArray().SelectMany(Walks))
            {
                yield return nested;
            }
        }
    }

    private static IEnumerable<string> Leaves(JsonElement element)
        => element.ValueKind switch
        {
            JsonValueKind.String => [element.GetString()!],
            JsonValueKind.Array => element.EnumerateArray().SelectMany(Leaves),
            _ => [],
        };

    private static ListRecord Record(TaskRow row, IEnumerable<string> fields)
    {
        var values = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
        foreach (var field in fields)
        {
            var json = field switch
            {
                "title" => JsonSerializer.Serialize(row.Title),
                "done" => row.Done ? "true" : "false",
                _ => "null",
            };
            values[field] = JsonDocument.Parse(json).RootElement.Clone();
        }

        return new ListRecord(row.Id, 2, values);
    }
}

/// <summary>A server fake for the form: reads a document, applies a patch (or refuses with a field error).</summary>
public sealed class FakeTaskDocumentGateway : IDocumentGateway
{
    private readonly FakeTaskListGateway _rows;

    public FakeTaskDocumentGateway(FakeTaskListGateway rows) => _rows = rows;

    /// <summary>The error the next save answers with, when a test sets one.</summary>
    public ApiError? FailNextPatch { get; set; }

    public List<string> Patches { get; } = [];

    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken)
        => Task.FromResult(TaskModel.Task());

    public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
        => Task.FromResult(new PermissionReply(
            PermissionSnapshot.Decode("""{"etag":"e1","data":{"doctypes":[{"doctype":"task","rights":{"create":true,"write":true,"submit":true,"cancel":true}}]}}"""),
            DocumentRights.All));

    public Task<DocumentReply> LoadAsync(string doctype, string id, CancellationToken cancellationToken)
    {
        var row = _rows.Rows.Find(candidate => candidate.Id == id)
            ?? throw new ApiRequestException(new ApiError { Code = "not_found", Message = "No such document", StatusCode = 404 });
        return Task.FromResult(new DocumentReply(TaskModel.Document(row), []));
    }

    public Task<DocumentReply> CreateAsync(string doctype, string bodyJson, CancellationToken cancellationToken)
    {
        using var document = JsonDocument.Parse(bodyJson);
        var title = document.RootElement.TryGetProperty("title", out var value) ? value.GetString() ?? "New" : "New";
        var row = new TaskRow("T-900", title, false);
        _rows.Rows.Add(row);
        return Task.FromResult(new DocumentReply(TaskModel.Document(row, version: 1), []));
    }

    public Task<DocumentReply> PatchAsync(string doctype, string id, string patchJson, int baseVersion, CancellationToken cancellationToken)
    {
        if (FailNextPatch is { } failure)
        {
            FailNextPatch = null;
            throw new ApiRequestException(failure);
        }

        Patches.Add(patchJson);
        var row = _rows.Rows.Find(candidate => candidate.Id == id)
            ?? throw new ApiRequestException(new ApiError { Code = "not_found", Message = "No such document", StatusCode = 404 });
        using var patch = JsonDocument.Parse(patchJson);
        var title = patch.RootElement.TryGetProperty("title", out var value) ? value.GetString() ?? row.Title : row.Title;
        var done = patch.RootElement.TryGetProperty("done", out var flag) ? flag.ValueKind == JsonValueKind.True : row.Done;
        var updated = row with { Title = title, Done = done };
        _rows.Rows[_rows.Rows.IndexOf(row)] = updated;
        return Task.FromResult(new DocumentReply(TaskModel.Document(updated, baseVersion + 1), []));
    }

    public Task DeleteAsync(string doctype, string id, int baseVersion, CancellationToken cancellationToken)
    {
        _ = _rows.Rows.RemoveAll(row => row.Id == id);
        return Task.CompletedTask;
    }

    public Task<DocumentReply> LifecycleAsync(string doctype, string id, LifecycleAction action, int baseVersion, string? reason, CancellationToken cancellationToken)
        => throw new NotSupportedException();

    public Task<DocumentReply> RenameAsync(string doctype, string id, string newName, int baseVersion, CancellationToken cancellationToken)
        => throw new NotSupportedException();

    public Controls.ILookupSource? LookupFor(string doctype, string field, Func<LookupSnapshot> snapshot) => null;
}

/// <summary>The host's dialogs, recorded; the workflows answer them by key, never by pointer.</summary>
public sealed class FakeSplitShell : IFormShell
{
    public List<(string DocType, string? Id)> Opened { get; } = [];
    public bool Confirm { get; set; } = true;

    public Task<bool> ConfirmAsync(string title, string message, string confirmLabel) => Task.FromResult(Confirm);

    public Task<string?> AskTextAsync(string title, string prompt, bool required) => Task.FromResult<string?>(null);

    public Task<bool> ConfirmDiscardAsync(IReadOnlyList<string> changed) => Task.FromResult(Confirm);

    public Task<ConflictChoice> ChooseConflictAsync(ConflictInfo info) => Task.FromResult(ConflictChoice.Keep);

    public Task ShowReferrersAsync(string title, IReadOnlyList<Referrer> referrers) => Task.CompletedTask;

    public void OpenDocument(string doctype, string? id) => Opened.Add((doctype, id));

    public void CloseTab()
    {
    }
}

/// <summary>List preferences held in memory (the list remembers columns per user and DocType).</summary>
public sealed class MemoryFilterStore : IFilterStore
{
    private readonly Dictionary<(string User, string DocType), ListPreferences> _values = [];

    public Task<ListPreferences> LoadAsync(string user, string doctype, CancellationToken cancellationToken)
        => Task.FromResult(_values.TryGetValue((user, doctype), out var value) ? value : ListPreferences.Empty);

    public Task SaveAsync(string user, string doctype, ListPreferences preferences, CancellationToken cancellationToken)
    {
        _values[(user, doctype)] = preferences;
        return Task.CompletedTask;
    }
}

/// <summary>The form's history over the workspace undo stack (the host's adapter, in tests).</summary>
public sealed class TestFormHistory : IFormHistory, IDisposable
{
    private readonly UndoStack _stack;
    private readonly Dictionary<string, ChildTableHistoryAdapter> _tables = new(StringComparer.Ordinal);

    public TestFormHistory(DocumentDraft draft)
    {
        Draft = draft;
        _stack = new UndoStack(draft);
    }

    /// <summary>The draft this history was built over (a probe compares it with the form's).</summary>
    public DocumentDraft Draft { get; }

    public bool CanUndo => _stack.CanUndo;
    public bool CanRedo => _stack.CanRedo;
    public string? UndoLabel => _stack.UndoLabel;
    public string? RedoLabel => _stack.RedoLabel;

    public event EventHandler? Changed
    {
        add => _stack.Changed += value;
        remove => _stack.Changed -= value;
    }

    public event EventHandler<string>? Cleared
    {
        add => _stack.Cleared += value;
        remove => _stack.Cleared -= value;
    }

    public ChildTableResult Undo() => _stack.TryUndo();

    public ChildTableResult Redo() => _stack.TryRedo();

    public void RunDerived(Action change) => _stack.RunDerived(change);

    public IChildTableHistory ForTable(string table)
    {
        if (!_tables.TryGetValue(table, out var adapter))
        {
            adapter = new ChildTableHistoryAdapter(_stack, table);
            _tables[table] = adapter;
        }

        return adapter;
    }

    public void SetGate(Func<string, ChildTableResult?> table, Func<string, ChildTableResult?>? field) => _stack.SetGate(table, field);

    public void Dispose() => _stack.Dispose();
}

/// <summary>
/// A ready-made arrangement in a headless window: the real list view over the fake server, the real form
/// view, the split, and a focus trace. Everything a keyboard test needs to press keys into.
/// </summary>
public sealed class SplitRig : IDisposable
{
    public const string User = "alice";

    public SplitRig(int rows = 3, FlowDirection flow = FlowDirection.LeftToRight, SplitMode? mode = null)
    {
        ListGateway = new FakeTaskListGateway();
        for (var index = 1; index <= rows; index++)
        {
            ListGateway.Rows.Add(new TaskRow("T-" + index.ToString(System.Globalization.CultureInfo.InvariantCulture), "Task " + index.ToString(System.Globalization.CultureInfo.InvariantCulture), false));
        }

        DocumentGateway = new FakeTaskDocumentGateway(ListGateway);
        Shell = new FakeSplitShell();
        Preferences = new InMemorySplitPreferenceStore();
        Trace = new FocusTrace();

        List = new ListView(
            new ListViewModel(ListGateway, Shell, new MemoryFilterStore(), "task", User, ListOptions()),
            lookups: null);
        Documents = new SplitDocumentHost(CreateForm);
        Split = new ListFormSplit(List, Documents, Preferences, User, WorkspaceKeymaps.Load("desktop"), Trace)
        {
            FlowDirection = flow,
        };

        Window = new Window { Width = 1240, Height = 720, Content = Split, FlowDirection = flow };
        Window.Show();
        Trace.Attach(Window);
        TestHost.Pump();
        LoadList();

        // A window that opens puts the focus somewhere: the arrangement opens on the list.
        _ = Split.FocusListPane();
        TestHost.Pump();
        if (mode is { } chosen)
        {
            Split.SetMode(chosen);
            TestHost.Pump();
        }
    }

    public FakeTaskListGateway ListGateway { get; }
    public FakeTaskDocumentGateway DocumentGateway { get; }
    public FakeSplitShell Shell { get; }
    public InMemorySplitPreferenceStore Preferences { get; }
    public FocusTrace Trace { get; }
    public ListView List { get; }
    public SplitDocumentHost Documents { get; }
    public ListFormSplit Split { get; }
    public Window Window { get; }

    /// <summary>The histories created for the forms of this rig, in creation order.</summary>
    public List<TestFormHistory> Histories { get; } = [];

    /// <summary>Loads (or reloads) the list and pumps until it is ready.</summary>
    public void LoadList()
    {
        var load = List.Model.LoadAsync();
        PumpUntil(() => load.IsCompleted, "the list to load");
        TestHost.Pump();
    }

    /// <summary>Sends one key press through the window's real input pipeline.</summary>
    public void Press(Key key, KeyModifiers modifiers = KeyModifiers.None)
    {
        Window.KeyPress(key, ToRaw(modifiers), PhysicalKey.None, null);
        Window.KeyRelease(key, ToRaw(modifiers), PhysicalKey.None, null);
        TestHost.Pump();
    }

    /// <summary>Opens a document and waits until the form has the focus on its first field.</summary>
    public async Task OpenAsync(string? id)
    {
        await Split.ShowAsync(id);
        PumpUntil(
            () => string.Equals(Split.CurrentDocumentId, id, StringComparison.Ordinal)
                  && Trace.Current?.StartsWith(FocusTrace.FieldPrefix, StringComparison.Ordinal) == true,
            "the form of " + (id ?? "a new document") + " to take the focus");
    }

    /// <summary>Types into the focused field and moves on with Tab (the editor commits on the move).</summary>
    public void TypeInto(string text)
    {
        Type(text);
        Press(Key.Tab);
    }

    /// <summary>Types text into the focused field (the headless platform's text input event).</summary>
    public void Type(string text)
    {
        Window.KeyTextInput(text);
        TestHost.Pump();
    }

    /// <summary>Raises a key event straight at the focused element (a control that swaps its own handlers).</summary>
    public void Raise(Key key, KeyModifiers modifiers = KeyModifiers.None)
    {
        var target = Window.FocusManager?.GetFocusedElement() as Interactive ?? Split;
        target.RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, RoutedEvent = InputElement.KeyDownEvent, Source = target });
        TestHost.Pump();
    }

    /// <summary>Pumps the dispatcher until <paramref name="done"/> or the attempt budget runs out (never a sleep).</summary>
    public static void PumpUntil(Func<bool> done, string what, int attempts = 2000)
    {
        for (var attempt = 0; attempt < attempts; attempt++)
        {
            if (done())
            {
                return;
            }

            TestHost.Pump();
            Thread.Yield();
        }

        throw new InvalidOperationException("Timed out waiting for " + what + ".");
    }

    public void Dispose()
    {
        Trace.Detach(Window);
        Window.Content = null;
        Window.Close();
        Split.Dispose();
        foreach (var history in Histories)
        {
            history.Dispose();
        }
    }

    /// <summary>The control with this automation id inside <paramref name="root"/> (the harness's one lookup).</summary>
    public static T? Find<T>(Avalonia.Visual root, string automationId)
        where T : Control
        => Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(root)
            .OfType<T>()
            .FirstOrDefault(control => Avalonia.Automation.AutomationProperties.GetAutomationId(control) == automationId);

    /// <summary>The focus trace entry of the control with this automation id, or null.</summary>
    public string? FocusedId => Trace.Current;

    /// <summary>The list options the tests use: no waits of any kind.</summary>
    public static ListViewModelOptions ListOptions() => new()
    {
        Culture = System.Globalization.CultureInfo.InvariantCulture,
        Zone = TimeZoneInfo.Utc,
        SearchDebounce = TimeSpan.Zero,
        Delay = (_, _) => Task.CompletedTask,
        Source = new ListSourceOptions { PageSize = 100 },
    };

    private DocumentFormViewModel CreateForm(string? id)
    {
        var model = new DocumentFormViewModel(DocumentGateway, "task", id, Shell);
        model.HistoryFactory = draft =>
        {
            var history = new TestFormHistory(draft);
            history.SetGate(_ => null, _ => null);
            Histories.Add(history);
            return history;
        };
        return model;
    }

    private static RawInputModifiers ToRaw(KeyModifiers modifiers)
    {
        var raw = RawInputModifiers.None;
        if (modifiers.HasFlag(KeyModifiers.Shift))
        {
            raw |= RawInputModifiers.Shift;
        }

        if (modifiers.HasFlag(KeyModifiers.Control))
        {
            raw |= RawInputModifiers.Control;
        }

        if (modifiers.HasFlag(KeyModifiers.Alt))
        {
            raw |= RawInputModifiers.Alt;
        }

        return raw;
    }
}
