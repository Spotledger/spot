using Spotledger.Demo.Mock.Contracts;

namespace Spotledger.Demo.Mock;

public enum AvailabilityState { Available, Blocked, NeedsContext, Restricted }
public sealed record Availability(AvailabilityState State, string Reason, string? ResolutionCommandId = null)
{
    public bool CanExecute => State == AvailabilityState.Available;
    public static Availability Ready { get; } = new(AvailabilityState.Available, "Available in the demo");
}
public enum DemoRole { ProcurementOfficer, Approver, WarehouseOperator, AccountsPayableClerk, SalesClerk, Cashier, Manager }
public enum EnforcementLevel { Guided, Standard, Strict }
public sealed record CommandContext(Guid CompanyId, Guid EstablishmentId, Guid? RecordId, string WorkspaceKey);
public sealed record DemoUser(Guid Id, string Name, DemoRole Role);
public sealed record Establishment(Guid Id, string Name);
public sealed record Company(Guid Id, string Name, IReadOnlyList<Establishment> Establishments);
public sealed record DemoTask(Guid Id, string Title, string Owner, DateTimeOffset Due, string CommandId);
public sealed record Kpi(string Label, decimal Value, string Unit, string Source, DateTimeOffset AsOf, string CommandId);
public sealed record Activity(string Description, string Owner, DateTimeOffset At, string CommandId);
public sealed record ProcessSummary(string Name, int Active, int Blocked, string CommandId);
public sealed record DemoRecord(Guid Id, string Number, string Title, long Version, string Note, bool IsOpen);
public sealed record HomeData(DemoUser User, Company Company, IReadOnlyList<DemoTask> Tasks,
    IReadOnlyList<Kpi> Kpis, IReadOnlyList<ProcessSummary> Processes, IReadOnlyList<Activity> Activity,
    IReadOnlyList<string> Shortcuts, IReadOnlyList<DemoRecord> Records, DateTimeOffset Now);
public sealed record DemoSettings(bool Governance, bool Maintenance, EnforcementLevel Enforcement);
public interface IBackend
{
    Task<ApiResult<HomeData>> GetHomeAsync();
    Task<ApiResult<HomeData>> SwitchRoleAsync(DemoRole role);
    Task<ApiResult<HomeData>> ResetAsync();
    Task<ApiResult<Availability>> GetAvailabilityAsync(string commandId, CommandContext context);
    Task<ApiResult<DemoRecord>> PatchRecordAsync(Guid id, long ifMatch, IReadOnlyDictionary<string, string?> mergePatch);
    Task<ApiResult<DemoSettings>> ConfigureAsync(DemoSettings settings);
}
