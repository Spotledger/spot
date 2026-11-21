using Spotledger.Client.Expressions;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>The repository's contract corpus and the sample app, located by walking up from
/// the test assembly: the client reads the same bytes the Rust resolver produced, so a
/// contract change that is not mirrored here fails a test instead of drifting.</summary>
internal static class Repo
{
    internal static string Root { get; } = FindRoot();

    internal static string UiExt { get; } = Path.Combine(Root, "contracts", "ui-ext");

    internal static string GoldenResolved { get; } =
        Path.Combine(Root, "crates", "framework", "spot-ui-ext", "tests", "golden", "sample-app.resolved.json");

    internal static string SampleUi { get; } = Path.Combine(UiExt, "examples", "sample-app", "ui.json");

    internal static string SampleContext { get; } = Path.Combine(UiExt, "examples", "sample-app", "context.json");

    internal static string Invalid { get; } = Path.Combine(UiExt, "examples", "invalid");

    internal static string Data(string name) => Path.Combine(AppContext.BaseDirectory, "Data", name);

    /// <summary>The instant every test uses in place of a clock (2023-11-14T22:13:20Z).</summary>
    internal static ExpressionDateTime Now { get; } = ExpressionDateTime.FromMicros(1_700_000_000_000_000)
        ?? throw new InvalidOperationException("the test instant is in range");

    private static string FindRoot()
    {
        DirectoryInfo? directory = new(AppContext.BaseDirectory);
        while (directory is not null)
        {
            if (Directory.Exists(Path.Combine(directory.FullName, "contracts", "ui-ext")))
            {
                return directory.FullName;
            }
            directory = directory.Parent;
        }
        throw new InvalidOperationException("the repository root (with contracts/ui-ext) was not found above the test assembly");
    }
}

/// <summary>The sample app's documents and the two DocTypes its items are scoped to.</summary>
internal static class Sample
{
    internal static string ResolvedJson => File.ReadAllText(Repo.GoldenResolved);

    internal static ExtensionFacts Facts => ExtensionFacts.FromContextJson(File.ReadAllText(Repo.SampleContext));

    internal static ExtensionSet Set() => ExtensionSet.Parse(ResolvedJson, Facts, "sample-app.resolved.json");

    internal static DocumentDraft TaskDraft()
    {
        DocumentDraft draft = new(new DocTypeModel("task", "Task", 3, false,
        [
            new FieldModel("name", "Name", FieldKind.Text),
            new FieldModel("status", "Status", FieldKind.Text),
            new FieldModel("assignee", "Assignee", FieldKind.Link, linkTo: "user"),
            new FieldModel("hours", "Hours", FieldKind.DecimalNumber, scale: 2)
        ]), "TASK-1", 4);
        draft.Set("name", new TextFieldValue("TASK-1"));
        draft.Set("status", new TextFieldValue("Open"));
        draft.Set("assignee", new LinkFieldValue("ali", "Ali Khan"));
        draft.Set("hours", new DecimalFieldValue(2.5m));
        return draft;
    }

    internal static DocumentDraft ProjectDraft()
    {
        DocumentDraft draft = new(new DocTypeModel("project", "Project", 2, false,
        [
            new FieldModel("status", "Status", FieldKind.Text),
            new FieldModel("owner", "Owner", FieldKind.Link, linkTo: "user"),
            new FieldModel("budget", "Budget", FieldKind.DecimalNumber, scale: 2)
        ]), "PROJ-1", 7);
        draft.Set("status", new TextFieldValue("Active"));
        draft.Set("owner", new LinkFieldValue("sara", "Sara Ahmed"));
        draft.Set("budget", new DecimalFieldValue(125000.50m));
        return draft;
    }

    /// <summary>The permissions the sample context declares (the server's permission summary).</summary>
    internal static IReadOnlyList<string> AllPermissions { get; } =
    [
        "project:read", "project:write", "task:read", "task:write", "task:create",
        "time_entry:read", "time_entry:create"
    ];

    internal static ExtensionContext TaskContext(
        DocumentDraft? draft = null,
        IEnumerable<string>? permissions = null,
        ExtensionServices? services = null)
    {
        _ = services;
        return ExtensionContext.FromDocument(
            draft ?? TaskDraft(),
            Repo.Now,
            user: "ali",
            permissions: permissions ?? AllPermissions,
            selection: ["TASK-1", "TASK-2"]);
    }

    internal static ExtensionContext ProjectContext(DocumentDraft? draft = null, IEnumerable<string>? permissions = null) =>
        ExtensionContext.FromDocument(draft ?? ProjectDraft(), Repo.Now, user: "ali", permissions: permissions ?? AllPermissions);
}

/// <summary>Records what a click asked for; answers with a scripted outcome.</summary>
internal sealed class FakeDispatcher : IActionDispatcher
{
    internal List<ActionRequest> Requests { get; } = [];

    internal Queue<ActionOutcome> Outcomes { get; } = new();

    internal Exception? Throw { get; set; }

    internal TaskCompletionSource? Hold { get; set; }

    public async Task<ActionOutcome> DispatchAsync(ActionRequest request, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(request);
        Requests.Add(request);
        if (Hold is not null)
        {
            await Hold.Task.WaitAsync(cancellationToken).ConfigureAwait(false);
        }
        if (Throw is not null)
        {
            throw Throw;
        }
        return Outcomes.Count > 0 ? Outcomes.Dequeue() : ActionOutcome.Refresh;
    }
}

/// <summary>Records the shell effects an answer was applied through.</summary>
internal sealed class FakeShell : IShellServices
{
    internal List<string> Calls { get; } = [];

    internal bool ConfirmAnswer { get; set; } = true;

    internal string? OutputDirectory { get; set; }

    public Task<bool> ConfirmAsync(string text, CancellationToken cancellationToken)
    {
        Calls.Add($"confirm:{text}");
        return Task.FromResult(ConfirmAnswer);
    }

    public Task ShowMessageAsync(string message, CancellationToken cancellationToken)
    {
        Calls.Add($"message:{message}");
        return Task.CompletedTask;
    }

    public Task RefreshAsync(string? doctype, string? documentId, CancellationToken cancellationToken)
    {
        Calls.Add($"refresh:{doctype}:{documentId}");
        return Task.CompletedTask;
    }

    public Task OpenAsync(string destination, IReadOnlyList<KeyValuePair<string, ExpressionValue>> filters, CancellationToken cancellationToken)
    {
        Calls.Add($"open:{destination}:{string.Join(',', filters.Select(filter => filter.Key))}");
        return Task.CompletedTask;
    }

    public Task OpenDocumentAsync(string documentId, CancellationToken cancellationToken)
    {
        Calls.Add($"document:{documentId}");
        return Task.CompletedTask;
    }

    public Task DownloadAsync(string reference, CancellationToken cancellationToken)
    {
        Calls.Add($"download:{reference}");
        return Task.CompletedTask;
    }
}

/// <summary>Answers a panel's declared queries from a fixed table.</summary>
internal sealed class FakeQueries : IQueryRunner
{
    internal Dictionary<string, QueryRows> Rows { get; } = new(StringComparer.Ordinal);

    internal Dictionary<string, QueryAggregate> Aggregates { get; } = new(StringComparer.Ordinal);

    internal Exception? Throw { get; set; }

    internal int RowCalls { get; private set; }

    internal int AggregateCalls { get; private set; }

    internal List<QueryPage> Pages { get; } = [];

    public Task<QueryRows> RunRowsAsync(string query, QueryPage page, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(page);
        RowCalls++;
        Pages.Add(page);
        if (Throw is not null)
        {
            return Task.FromException<QueryRows>(Throw);
        }
        QueryRows all = Rows.TryGetValue(query, out QueryRows? rows) ? rows : QueryRows.Empty;
        IReadOnlyList<IReadOnlyList<string>> slice = [.. all.Rows.Skip(page.Offset).Take(page.Limit)];
        bool hasMore = page.Offset + slice.Count < all.Rows.Count;
        return Task.FromResult(new QueryRows(all.Columns, slice, hasMore));
    }

    public Task<QueryAggregate> RunAggregateAsync(string query, CancellationToken cancellationToken)
    {
        AggregateCalls++;
        if (Throw is not null)
        {
            return Task.FromException<QueryAggregate>(Throw);
        }
        return Task.FromResult(Aggregates.TryGetValue(query, out QueryAggregate? value)
            ? value
            : new QueryAggregate(new IntValue(0)));
    }

    /// <summary>The sample app's three declared queries, answered plausibly.</summary>
    internal static FakeQueries Sample()
    {
        FakeQueries queries = new();
        queries.Rows["sample.recent_time_entries"] = new QueryRows(
            ["work_date", "person", "hours"],
            [
                ["2023-11-13", "Ali Khan", "2.50"],
                ["2023-11-12", "Sara Ahmed", "1.25"],
                ["2023-11-11", "Ali Khan", "4.00"]
            ],
            false);
        queries.Aggregates["sample.open_tasks"] = new QueryAggregate(new IntValue(7));
        queries.Aggregates["sample.task_count"] = new QueryAggregate(new IntValue(23));
        return queries;
    }
}

/// <summary>Collects the hosts a shell registered, for the slot-registration tests.</summary>
internal sealed class FakeRegistry : IShellSlotRegistry
{
    internal Dictionary<string, Func<string?, object?>> Factories { get; } = new(StringComparer.Ordinal);

    internal List<(string Code, string Message)> Notices { get; } = [];

    public void RegisterSlotHost(string slot, Func<string?, object?> hostFactory) => Factories[slot] = hostFactory;

    public void ShowNotice(string code, string message) => Notices.Add((code, message));
}

/// <summary>Reads the corpus file's <c>expect: &lt;code&gt;</c> header.</summary>
internal static class Corpus
{
    internal static IReadOnlyList<(string File, string Code)> InvalidFiles()
    {
        List<(string, string)> files = [];
        foreach (string path in Directory.GetFiles(Repo.Invalid, "*.json").Order(StringComparer.Ordinal))
        {
            string text = File.ReadAllText(path);
            System.Text.RegularExpressions.Match match =
                System.Text.RegularExpressions.Regex.Match(text, "expect: (uiext\\.[a-z_]+)", System.Text.RegularExpressions.RegexOptions.CultureInvariant);
            Assert.True(match.Success, $"{path} carries an `expect: uiext.<code>` header");
            files.Add((path, match.Groups[1].Value));
        }
        return files;
    }
}
