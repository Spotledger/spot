using System.Text.Json.Nodes;
using Spotledger.Client.Api;
using Spotledger.Client.Desktop.Hosting;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;
using Spotledger.Client.Workspace;

namespace Spotledger.Client.Shell.Tests.Wiring;

/// <summary>A clock tests move by hand; its timers fire from <see cref="Advance"/>.</summary>
internal sealed class ManualClock : TimeProvider
{
    private readonly List<ManualTimer> _timers = [];
    private DateTimeOffset _now = new(2026, 10, 24, 9, 0, 0, TimeSpan.Zero);

    public override DateTimeOffset GetUtcNow() => _now;

    public override ITimer CreateTimer(TimerCallback callback, object? state, TimeSpan dueTime, TimeSpan period)
    {
        var timer = new ManualTimer(this, callback, state);
        _ = timer.Change(dueTime, period);
        _timers.Add(timer);
        return timer;
    }

    public void Advance(TimeSpan span)
    {
        var target = _now + span;
        while (_timers.Where(t => t.Due is not null && t.Due <= target).OrderBy(t => t.Due).FirstOrDefault() is { } next)
        {
            _now = next.Due!.Value > _now ? next.Due.Value : _now;
            next.Fire();
        }

        _now = target;
    }

    private sealed class ManualTimer(ManualClock clock, TimerCallback callback, object? state) : ITimer
    {
        public DateTimeOffset? Due { get; private set; }

        public bool Change(TimeSpan dueTime, TimeSpan period)
        {
            Due = dueTime == Timeout.InfiniteTimeSpan ? null : clock._now + dueTime;
            return true;
        }

        public void Fire()
        {
            Due = null;
            callback(state);
        }

        public void Dispose() => Due = null;

        public ValueTask DisposeAsync()
        {
            Due = null;
            return ValueTask.CompletedTask;
        }
    }
}

/// <summary>An OS credential store stand-in that is not the in-memory fallback, so drafts reach the disk.</summary>
internal sealed class FakeOsSecretStore : ISecretStore
{
    private readonly Dictionary<string, string> _secrets = [];

    public IReadOnlyDictionary<string, string> Secrets => _secrets;

    public Task<string?> GetSecretAsync(string key, CancellationToken cancellationToken = default)
        => Task.FromResult(_secrets.GetValueOrDefault(key));

    public Task SetSecretAsync(string key, string value, CancellationToken cancellationToken = default)
    {
        _secrets[key] = value;
        return Task.CompletedTask;
    }

    public Task DeleteSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        _ = _secrets.Remove(key);
        return Task.CompletedTask;
    }
}

/// <summary>A server for one DocType that records every call, so a test can assert nothing was sent.</summary>
internal sealed class WiringGateway : IDocumentGateway
{
    public DocumentRights Rights { get; set; } = DocumentRights.All;

    public string MaskedField { get; set; } = string.Empty;

    public string State { get; set; } = "draft";

    public List<string> Calls { get; } = [];

    public bool SentWrite => Calls.Any(c => c.StartsWith("create", StringComparison.Ordinal) || c.StartsWith("patch", StringComparison.Ordinal));

    public static DocTypeModel Model()
    {
        var line = new DocTypeModel("line", "Line", 1, false,
        [
            new FieldModel("item", "Item", FieldKind.Text),
            new FieldModel("qty", "Qty", FieldKind.DecimalNumber, scale: 2, precision: 18),
        ]);
        return new DocTypeModel("invoice", "Invoice", 1, true,
        [
            new FieldModel("title", "Title", FieldKind.Text, required: true),
            new FieldModel("qty", "Quantity", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("total", "Total", FieldKind.DecimalNumber, scale: 2, precision: 18, readOnly: true),
            new FieldModel("secret_note", "Private note", FieldKind.Text),
            new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line"),
        ],
        new Dictionary<string, DocTypeModel> { ["items"] = line });
    }

    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken) => Task.FromResult(Model());

    public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
    {
        var fields = MaskedField.Length == 0 ? "{}" : $$"""{"masked":["{{MaskedField}}"]}""";
        var snapshot = PermissionSnapshot.Decode(
            $$$"""{"etag":"e1","data":{"doctypes":[{"doctype":"invoice","rights":{"create":true,"write":{{{Rights.Write.ToString().ToLowerInvariant()}}}},"fields":{{{fields}}}}]}}""");
        return Task.FromResult(new PermissionReply(snapshot, Rights));
    }

    public Task<DocumentReply> LoadAsync(string doctype, string id, CancellationToken cancellationToken)
    {
        Calls.Add("load " + id);
        var document = new JsonObject
        {
            ["id"] = id,
            ["version"] = 3,
            ["_state"] = State,
            ["title"] = "October",
            ["qty"] = "1.00",
            ["total"] = "10.00",
            ["secret_note"] = "plain-secret-note",
            ["items"] = new JsonArray
            {
                new JsonObject { ["id"] = "r1", ["item"] = "Bolt", ["qty"] = "2.00" },
                new JsonObject { ["id"] = "r2", ["item"] = "Nut", ["qty"] = "3.00" },
            },
        };
        return Task.FromResult(new DocumentReply(document.ToJsonString(), []));
    }

    public Task<DocumentReply> CreateAsync(string doctype, string bodyJson, CancellationToken cancellationToken)
    {
        Calls.Add("create");
        throw new InvalidOperationException("A test must not write.");
    }

    public Task<DocumentReply> PatchAsync(string doctype, string id, string patchJson, int baseVersion, CancellationToken cancellationToken)
    {
        Calls.Add("patch " + id);
        throw new InvalidOperationException("A test must not write.");
    }

    public Task DeleteAsync(string doctype, string id, int baseVersion, CancellationToken cancellationToken)
    {
        Calls.Add("delete " + id);
        throw new InvalidOperationException("A test must not write.");
    }

    public Task<DocumentReply> LifecycleAsync(string doctype, string id, LifecycleAction action, int baseVersion, string? reason, CancellationToken cancellationToken)
    {
        Calls.Add("lifecycle " + id);
        throw new InvalidOperationException("A test must not write.");
    }

    public Task<DocumentReply> RenameAsync(string doctype, string id, string newName, int baseVersion, CancellationToken cancellationToken)
    {
        Calls.Add("rename " + id);
        throw new InvalidOperationException("A test must not write.");
    }

    public Spotledger.Client.Controls.ILookupSource? LookupFor(string doctype, string field, Func<LookupSnapshot> snapshot) => null;
}

/// <summary>A signed-in shell with the workspace wired over a temp draft folder, a fake credential store and a fake server.</summary>
internal sealed class Rig : IDisposable
{
    private readonly string _folder = Path.Combine(Path.GetTempPath(), "spot-wiring-" + Guid.NewGuid().ToString("N"));

    public Rig(WiringGateway? gateway = null, string user = TestHarness.DefaultUser)
    {
        Gateway = gateway ?? new WiringGateway();
        Harness = new TestHarness();
        Secrets = new FakeOsSecretStore();
        Clock = new ManualClock();
        Drafts = new DraftSessionHost(
            _ => new FileDraftStore(new FileDraftStoreOptions { Root = Root }, Secrets),
            Path.Combine(Root, "pending-wipe"),
            Clock,
            new AutosaveOptions { Post = action => action() });
        Host = new WorkspaceHost(Harness.Shell, Drafts, Gateway, () => TestHarness.ServerUrl, Clock, post: action => action(), undoSchedule: _ => { });
        User = user;
    }

    public string User { get; }

    public string Root => _folder;

    public WiringGateway Gateway { get; }

    public TestHarness Harness { get; }

    public FakeOsSecretStore Secrets { get; }

    public ManualClock Clock { get; }

    public DraftSessionHost Drafts { get; }

    public WorkspaceHost Host { get; }

    public ShellViewModel Shell => Harness.Shell;

    public IEnumerable<string> DraftFiles => Directory.Exists(Root)
        ? Directory.EnumerateFiles(Root, "*.draft", SearchOption.AllDirectories)
        : [];

    /// <summary>Signs in through the fake server and waits until the draft scope is open.</summary>
    public async Task SignInAsync()
    {
        await Harness.CheckServerAsync();
        await Harness.OpenLoginAsync();
        await Harness.SignInAsync(User);
        await TestHarness.WaitUntilAsync(() => Drafts.Scope is not null, "the draft scope to open");
    }

    /// <summary>Opens a document and waits for it to be loaded and offered a stored draft.</summary>
    public async Task<DocumentFormViewModel> OpenAsync(string id = "INV-1", string? newDraftId = null)
    {
        var tab = Host.OpenDocument("invoice", id, newDraftId) ?? throw new InvalidOperationException("The document did not open.");
        var form = (DocumentFormViewModel)tab.Content!;
        await TestHarness.WaitUntilAsync(() => form.Phase == FormPhase.Ready && form.Draft is not null, "the form to load");
        await Task.Delay(30);
        return form;
    }

    public void Dispose()
    {
        Host.Dispose();
        Drafts.Dispose();
        Harness.Dispose();
        if (Directory.Exists(_folder))
        {
            try
            {
                Directory.Delete(_folder, recursive: true);
            }
            catch (IOException)
            {
                // Temp folder sweep is best effort.
            }
        }
    }
}
