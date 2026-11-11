using System.Diagnostics;
using Spotledger.Client.Extensions;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>The documented budget: model-resolving a toolbar of twenty actions with
/// expressions stays under 16 ms p95 on the reference runner (measured, not raw wall time in
/// a unit test's head), and the work is counted as well: one evaluation per expression, no
/// more, however often the slot is refreshed.</summary>
public sealed class PerfTests
{
    private const int Actions = 20;

    [Fact]
    public void ATwentyActionToolbarResolvesInsideTheDocumentedBudget()
    {
        ExtensionSet set = BuildToolbar();
        Assert.Equal(Actions, set.Actions.Count);
        using ExtensionContext context = CountingContext(out CountingReader reader);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        Assert.Equal(Actions, view.Entries.Count);

        PerfBudget budgets = PerfBudget.Load(Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml"));
        PerfMeasurement measurement = PerfMeter.Measure(() =>
        {
            using SlotView resolved = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
            foreach (SlotEntry entry in resolved.Entries)
            {
                Assert.True(entry.IsVisible);
            }
        }, samples: 20, warmup: 3);
        new Budget(budgets).Assert("uiext.resolve.toolbar", measurement);

        // Counted budget: each action's visible_if and enabled_if reads exactly one field, so
        // building a view costs Actions * 2 reads and a refresh costs the same again — never
        // one evaluation per expression per repaint of the same state.
        int before = reader.Reads;
        using SlotView again = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        Assert.Equal(Actions, again.Entries.Count);
        Assert.Equal(Actions * 2, reader.Reads - before);
        before = reader.Reads;
        again.Refresh();
        Assert.Equal(Actions * 2, reader.Reads - before);
        before = reader.Reads;
        again.Refresh();
        Assert.Equal(Actions * 2, reader.Reads - before);
    }

    [Fact]
    public void AHiddenActionIsNotEvaluatedTwice()
    {
        ExtensionSet set = BuildToolbar();
        using ExtensionContext context = CountingContext(out CountingReader reader, status: "Closed");
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        Assert.Empty(view.Entries);
        Assert.Equal(Actions, reader.Reads);
    }

    private static ExtensionSet BuildToolbar()
    {
        System.Text.StringBuilder actions = new();
        for (int index = 0; index < Actions; index++)
        {
            if (index > 0)
            {
                actions.Append(',');
            }
            actions.Append(System.Globalization.CultureInfo.InvariantCulture, $$"""
                { "id": "app{{index}}.a", "origin": "app:app{{index}}", "label": "Action {{index}}", "slot": "doc.toolbar", "doctype": "task", "order": {{index}},
                  "visible_if": "doc.status == 'Open'", "enabled_if": "not is_empty(doc.assignee)",
                  "call": { "doctype": "task", "method": "start_work" } }
                """);
        }
        string document = "{ \"contract_version\": \"0.1.0\", \"requires_shell\": \"0.1.0\", \"actions\": [" + actions + "] }";
        ExtensionSet set = ExtensionSet.Parse(document, Sample.Facts);
        Assert.Empty(set.Diagnostics);
        return set;
    }

    private static ExtensionContext CountingContext(out CountingReader reader, string status = "Open")
    {
        CountingReader counter = new(status);
        reader = counter;
        return new ExtensionContext(Repo.Now, permissions: Sample.AllPermissions, document: counter.Read);
    }

    /// <summary>A document reader that counts how many field reads an evaluation made.</summary>
    internal sealed class CountingReader(string status)
    {
        internal int Reads { get; private set; }

        internal Spotledger.Client.Metadata.FieldValue? Read(string name)
        {
            Reads++;
            return name switch
            {
                "status" => new Spotledger.Client.Metadata.TextFieldValue(status),
                "assignee" => new Spotledger.Client.Metadata.LinkFieldValue("ali", "Ali Khan"),
                _ => null
            };
        }
    }
}
