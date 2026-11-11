using Spotledger.Demo.Mock.Contracts;

namespace Spotledger.Demo.Mock;

/// <summary>Deterministic demo data only. Never promote this mock to production.</summary>
public sealed class MockBackend : IBackend
{
    public static Guid CompanyId { get; } = Guid.Parse("01994c50-0000-7000-8000-000000000001");
    public static Guid RecordId { get; } = Guid.Parse("01994c50-0000-7000-8000-000000000030");
    private readonly IClock clock;
    private readonly Dictionary<Guid, DemoRecord> records = [];
    private DemoRole role = DemoRole.ProcurementOfficer;
    private DemoSettings settings = new(true, false, EnforcementLevel.Guided);
    public static IReadOnlyList<string> RoleNames { get; } =
        ["Procurement officer", "Approver", "Warehouse operator", "Accounts payable clerk", "Sales clerk", "Cashier", "Manager"];
    public MockBackend(IClock? clock = null)
    {
        this.clock = clock ?? new FixedClock(new DateTimeOffset(2026, 9, 30, 9, 0, 0, TimeSpan.Zero));
        Seed();
    }
    private void Seed()
    {
        records.Clear();
        records.Add(RecordId, new(RecordId, "DEMO-PO-001", "Purchase order", 1, "Seed note", true));
        var id = Guid.Parse("01994c50-0000-7000-8000-000000000031");
        records.Add(id, new(id, "DEMO-SI-001", "Sales invoice", 1, "Demo draft", false));
    }
    private HomeData Home()
    {
        var user = new DemoUser(Users[(int)role], $"Demo {RoleNames[(int)role]}", role);
        var shortcut = Shortcuts[(int)role];
        DemoTask[] tasks = role == DemoRole.Manager ? [] :
            [new(TaskIds[(int)role], TaskTitles[(int)role], user.Name, clock.Now.AddHours(-1), shortcut),
             new(Guid.Parse("01994c50-0000-7000-8000-000000000070"), "Review supporting evidence", user.Name, clock.Now.AddDays(1), "workspace.tasks")];
        return new(user,
            new(CompanyId, "Demo Meridian Trading", [new(Guid.Parse("01994c50-0000-7000-8000-000000000002"), "Head office"),
                new(Guid.Parse("01994c50-0000-7000-8000-000000000003"), "Harbour store")]), tasks,
            [new("Purchases this month", 832000m, "PKR", "Mock balance table", clock.Now, "report.purchases"),
             new("Stock value", 2560000m, "PKR", "Mock balance table", clock.Now, "report.stock"),
             new("Outstanding receivables", 672000m, "PKR", "Mock replica", clock.Now, "report.receivables")],
            [new("Procure to Pay", 3, 2, "workspace.processes")],
            [new("Purchase order saved as draft", "Demo procurement officer", clock.Now.AddMinutes(-18), $"record.{RecordId}"),
             new("Sales invoice drafted", "Demo sales clerk", clock.Now.AddMinutes(-35), "workspace.sell")],
            [shortcut, "workspace.tasks", "workspace.reports"], records.Values.ToArray(), clock.Now);
    }
    // Hard-coded v7-style UUID fixtures; roles and records never get random identities.
    private static readonly Guid[] Users =
        [Guid.Parse("01994c50-0000-7000-8000-000000000010"),
         Guid.Parse("01994c50-0000-7000-8000-000000000011"),
         Guid.Parse("01994c50-0000-7000-8000-000000000012"),
         Guid.Parse("01994c50-0000-7000-8000-000000000013"),
         Guid.Parse("01994c50-0000-7000-8000-000000000014"),
         Guid.Parse("01994c50-0000-7000-8000-000000000015"),
         Guid.Parse("01994c50-0000-7000-8000-000000000016")];
    private static readonly Guid[] TaskIds =
        [Guid.Parse("01994c50-0000-7000-8000-000000000040"),
         Guid.Parse("01994c50-0000-7000-8000-000000000041"),
         Guid.Parse("01994c50-0000-7000-8000-000000000042"),
         Guid.Parse("01994c50-0000-7000-8000-000000000043"),
         Guid.Parse("01994c50-0000-7000-8000-000000000044"),
         Guid.Parse("01994c50-0000-7000-8000-000000000045"),
         Guid.Parse("01994c50-0000-7000-8000-000000000046")];
    private static readonly string[] Shortcuts = ["create.request", "workspace.approvals", "create.receipt", "create.invoice",
        "create.sale", "workspace.pos", "workspace.reports"];
    private static readonly string[] TaskTitles = ["Source purchase request", "Approve purchase order", "Receive expected goods",
        "Review supplier invoice", "Prepare customer invoice", "Open checkout shift", "Review process health"];
    public Task<ApiResult<HomeData>> GetHomeAsync() => Task.FromResult(ApiResults.Ok(Home(), "demo.home"));
    public Task<ApiResult<HomeData>> SwitchRoleAsync(DemoRole role)
    {
        this.role = role;
        return GetHomeAsync();
    }
    public Task<ApiResult<HomeData>> ResetAsync()
    {
        role = DemoRole.ProcurementOfficer;
        settings = new(true, false, EnforcementLevel.Guided);
        Seed();
        return GetHomeAsync();
    }
    public Task<ApiResult<DemoSettings>> ConfigureAsync(DemoSettings settings)
    {
        this.settings = settings;
        return Task.FromResult(ApiResults.Ok(settings, "demo.configure"));
    }
    public Task<ApiResult<Availability>> GetAvailabilityAsync(string commandId, CommandContext context)
    {
        // Canned availability examples, not a production authorization or process model.
        var result = commandId switch
        {
            "document.save" or "document.post" when context.RecordId is null =>
                new(AvailabilityState.NeedsContext, "Open a record first.", $"record.{RecordId}"),
            "document.post" when settings.Maintenance =>
                new(AvailabilityState.Blocked, "Demo maintenance mode prevents posting.", "demo.maintenance"),
            "document.post" when settings.Governance =>
                new(AvailabilityState.Blocked, "Goods receipt is required before this invoice can proceed.", "create.receipt"),
            "document.post" => new(AvailabilityState.Blocked, "Posting is delivered in part 2; this is a placeholder document."),
            "tools.admin" when role != DemoRole.Manager =>
                new(AvailabilityState.Restricted, "The demo Manager role is required.", "demo.role.Manager"),
            "create.receipt" when role is not (DemoRole.WarehouseOperator or DemoRole.Manager) =>
                new(AvailabilityState.Restricted, "The demo Warehouse operator role is required.", "demo.role.WarehouseOperator"),
            _ => Availability.Ready
        };
        return Task.FromResult(ApiResults.Ok(result, commandId));
    }
    public Task<ApiResult<DemoRecord>> PatchRecordAsync(Guid id, long ifMatch, IReadOnlyDictionary<string, string?> mergePatch)
    {
        if (!records.TryGetValue(id, out var record))
            return Task.FromResult(ApiResults.Fail<DemoRecord>("demo.note.save", "document.not_found", "Demo record was not found."));
        if (record.Version != ifMatch)
            return Task.FromResult(ApiResults.Fail<DemoRecord>("demo.note.save", "document.version_conflict", "The record changed. Reopen it before saving."));
        if (mergePatch.Keys.Any(key => key != "note"))
            return Task.FromResult(ApiResults.Fail<DemoRecord>("demo.note.save", "document.validation", "Only the demo note is editable.",
                new FieldError("/note", "field.invalid", "Use the note field.")));
        record = record with { Version = record.Version + 1, Note = mergePatch.GetValueOrDefault("note", record.Note) ?? "" };
        records[id] = record;
        return Task.FromResult(ApiResults.Ok(record, "demo.note.save", record.Version.ToString(System.Globalization.CultureInfo.InvariantCulture)));
    }
}
