using Spotledger.Client.Api;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Workspace.Tests;

/// <summary>A clock tests move by hand; its timers fire from <see cref="Advance"/>.</summary>
public sealed class ManualClock : TimeProvider
{
    private readonly List<ManualTimer> _timers = [];
    private DateTimeOffset _now = new(2026, 10, 23, 9, 0, 0, TimeSpan.Zero);

    public override DateTimeOffset GetUtcNow() => _now;

    public override ITimer CreateTimer(TimerCallback callback, object? state, TimeSpan dueTime, TimeSpan period)
    {
        var timer = new ManualTimer(this, callback, state);
        _ = timer.Change(dueTime, period);
        _timers.Add(timer);
        return timer;
    }

    /// <summary>Moves time forward, firing due timers in order.</summary>
    public void Advance(TimeSpan span)
    {
        var target = _now + span;
        while (true)
        {
            var next = _timers.Where(t => t.Due is not null && t.Due <= target).OrderBy(t => t.Due).FirstOrDefault();
            if (next is null)
            {
                break;
            }

            _now = next.Due!.Value > _now ? next.Due.Value : _now;
            next.Fire();
        }

        _now = target;
    }

    public int ActiveTimers => _timers.Count(t => t.Due is not null);

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

/// <summary>An OS credential store stand-in that is not the in-memory fallback.</summary>
public sealed class FakeOsSecretStore : ISecretStore
{
    private readonly Dictionary<string, string> _secrets = [];

    public bool Unavailable { get; set; }

    public IReadOnlyDictionary<string, string> Secrets => _secrets;

    public Task<string?> GetSecretAsync(string key, CancellationToken cancellationToken = default)
        => Unavailable ? throw new InvalidOperationException("no secret service") : Task.FromResult(_secrets.GetValueOrDefault(key));

    public Task SetSecretAsync(string key, string value, CancellationToken cancellationToken = default)
    {
        if (Unavailable)
        {
            throw new InvalidOperationException("no secret service");
        }

        _secrets[key] = value;
        return Task.CompletedTask;
    }

    public Task DeleteSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        _ = _secrets.Remove(key);
        return Task.CompletedTask;
    }
}

/// <summary>A temp folder that goes away.</summary>
public sealed class TempFolder : IDisposable
{
    public string Path { get; } = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "spot-drafts-" + Guid.NewGuid().ToString("N"));

    public void Dispose()
    {
        if (Directory.Exists(Path))
        {
            Directory.Delete(Path, recursive: true);
        }
    }
}

public static class Models
{
    public static readonly DraftScope Scope = new("https://erp.example.test", "alice");

    public static DocTypeModel Invoice()
    {
        var line = new DocTypeModel("line", "Line", 1, false,
        [
            new FieldModel("item", "Item", FieldKind.Text),
            new FieldModel("qty", "Quantity", FieldKind.DecimalNumber, scale: 2, precision: 18),
        ]);
        return new DocTypeModel("invoice", "Invoice", 1, true,
        [
            new FieldModel("title", "Title", FieldKind.Text),
            new FieldModel("qty", "Quantity", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("ssn", "Tax number", FieldKind.Text),
            new FieldModel("api_key", "API key", FieldKind.Secret),
            new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line"),
        ],
        new Dictionary<string, DocTypeModel> { ["items"] = line });
    }

    /// <summary>A loaded invoice draft (id INV-1, version 3, two lines).</summary>
    public static DocumentDraft LoadedInvoice(int version = 3, string title = "October")
    {
        var draft = new DocumentDraft(Invoice());
        draft.LoadFrom($$"""{"id":"INV-1","version":{{version}},"title":"{{title}}","qty":"1.00","amount":"10.00","ssn":"123-45","items":[{"id":"r1","idx":1,"item":"Bolt","qty":"2.00"},{"id":"r2","idx":2,"item":"Nut","qty":"3.00"}]}""");
        return draft;
    }

    public static void Mask(DocumentDraft draft, string field)
        => draft.ApplyPermissions(PermissionSnapshot.Decode(
            $$$"""{"etag":"e1","data":{"doctypes":[{"doctype":"invoice","rights":{"create":true,"write":true},"fields":{"masked":["{{{field}}}"]}}]}}"""));
}
