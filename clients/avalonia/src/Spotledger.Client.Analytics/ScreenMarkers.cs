namespace Spotledger.Client.Analytics;

/// <summary>
/// Marks the view model (or view) that implements a screen listed in the screens manifest
/// (tools/ui-coverage). The coverage tracker finds it by a static scan of source text, so the
/// id must be a string literal: <c>[ScreenId("sales.invoice.form")]</c>. The same id is what
/// <see cref="UsageAnalytics.ScreenOpened"/> reports.
/// </summary>
[AttributeUsage(AttributeTargets.Class | AttributeTargets.Struct, AllowMultiple = true, Inherited = false)]
public sealed class ScreenIdAttribute : Attribute
{
    public ScreenIdAttribute(string id)
    {
        Id = id;
    }

    public string Id { get; }
}

/// <summary>
/// Marks a headless UI test class or method as covering a screen, optionally naming the states
/// it exercises (<c>"empty"</c>, <c>"error"</c>, ...). With no states it covers <c>default</c>.
/// Ids and states are string literals so the static scan can read them.
/// </summary>
[AttributeUsage(AttributeTargets.Class | AttributeTargets.Method, AllowMultiple = true, Inherited = false)]
public sealed class CoversScreenAttribute : Attribute
{
    public CoversScreenAttribute(string id, params string[] states)
    {
        Id = id;
        States = states;
    }

    public string Id { get; }

    public IReadOnlyList<string> States { get; }
}
