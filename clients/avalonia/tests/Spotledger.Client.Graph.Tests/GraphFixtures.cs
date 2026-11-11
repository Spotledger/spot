using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Spotledger.Client.Graph;

namespace Spotledger.Client.Graph.Tests;

/// <summary>A clock the test moves by hand: no view logic reads the wall clock.</summary>
public sealed class FixedClock : IGraphClock
{
    /// <inheritdoc />
    public DateTimeOffset Now { get; set; } = new(2026, 1, 1, 12, 0, 0, TimeSpan.Zero);
}

/// <summary>The server-backed editor fake: records every question and update, and answers
/// with the scripted decision. Cycle validity is the fake's answer here, exactly as the real
/// graph query owns it — the view under test never decides.</summary>
public sealed class FakeHierarchyEditor : IHierarchyEditor
{
    /// <summary>Every validity question the view asked.</summary>
    public List<ReparentRequest> Checks { get; } = [];

    /// <summary>Every update the view applied.</summary>
    public List<ReparentRequest> Applied { get; } = [];

    /// <summary>The decision the fake server answers with (allow by default).</summary>
    public Func<ReparentRequest, ReparentDecision> Decide { get; set; } = _ => ReparentDecision.Allow;

    /// <summary>The parent each node had before the update (the plane the change reports).</summary>
    public Dictionary<GraphNodeKey, GraphNodeKey?> Parents { get; } = [];

    /// <inheritdoc />
    public Task<ReparentDecision> CheckAsync(ReparentRequest request, CancellationToken cancellationToken)
    {
        Checks.Add(request);
        return Task.FromResult(Decide(request));
    }

    /// <inheritdoc />
    public Task<ReparentChange> ApplyAsync(ReparentRequest request, CancellationToken cancellationToken)
    {
        var decision = Decide(request);
        if (!decision.Allowed)
        {
            throw new HierarchyEditRefusedException(decision.Reason ?? "refused");
        }

        Applied.Add(request);
        Parents.TryGetValue(request.Node, out var previous);
        Parents[request.Node] = request.Target;
        return Task.FromResult(new ReparentChange(request.Node, previous, request.Target));
    }
}

/// <summary>Records the edits a view registers on the workspace undo stack.</summary>
public sealed class RecordingUndo : IWorkspaceUndo
{
    /// <summary>The registered edits, oldest first.</summary>
    public List<UndoableEdit> Edits { get; } = [];

    /// <inheritdoc />
    public void Record(UndoableEdit edit) => Edits.Add(edit);

    /// <summary>Runs the most recent edit's inverse.</summary>
    public Task UndoLast(CancellationToken cancellationToken)
    {
        var edit = Edits[^1];
        return edit.Undo(cancellationToken);
    }
}

/// <summary>Records the documents a view opened through the navigation seam.</summary>
public sealed class RecordingNavigation : IWorkspaceNavigation
{
    /// <summary>The opened record keys.</summary>
    public List<GraphNodeKey> Opened { get; } = [];

    /// <inheritdoc />
    public Task OpenDocumentAsync(GraphNodeKey key, CancellationToken cancellationToken)
    {
        Opened.Add(key);
        return Task.CompletedTask;
    }
}

/// <summary>Builders and the keyboard helper shared by the graph tests.</summary>
public static class GraphFixtures
{
    /// <summary>The first account of the sample tree.</summary>
    public static GraphNodeKey RootKey => new("Account", "000000");

    /// <summary>The root of the organisation sample.</summary>
    public static GraphNodeKey OrganisationRoot => new("Company", "head");

    /// <summary>Shows a hierarchy over a source and pumps until its root page arrived.</summary>
    public static SpotHierarchy MakeHierarchy(
        IHierarchySource source,
        int pageSize = 100,
        bool breadcrumb = true,
        double width = 600,
        double height = 400,
        int pumps = 2)
    {
        var tree = new SpotHierarchy
        {
            Source = source,
            PageSize = pageSize,
            ShowBreadcrumb = breadcrumb,
            Clock = new FixedClock(),
        };
        TestHost.Show(tree, width, height);
        for (var i = 0; i < pumps; i++)
        {
            TestHost.Pump();
        }

        return tree;
    }

    /// <summary>Raises a synthesized key through the real input pipeline.</summary>
    public static void Press(Control control, Key key, KeyModifiers modifiers = KeyModifiers.None)
        => control.RaiseEvent(new KeyEventArgs
        {
            Key = key,
            KeyModifiers = modifiers,
            RoutedEvent = InputElement.KeyDownEvent,
        });

    /// <summary>Raises a synthesized printable character (type-ahead).</summary>
    public static void Type(Control control, string text)
    {
        foreach (var character in text)
        {
            control.RaiseEvent(new KeyEventArgs
            {
                Key = Key.None,
                KeySymbol = character.ToString(),
                RoutedEvent = InputElement.KeyDownEvent,
            });
        }
    }

    /// <summary>A small organisation tree with one redacted department.</summary>
    public static InMemoryHierarchySource OrganisationSource() => new(HierarchySamples.Organisation());

    /// <summary>A lineage snapshot: order → receipt → invoice → payment, with a locked node.</summary>
    public static GraphSnapshot SupplyChain()
    {
        var order = new GraphNodeKey("SalesOrder", "SO-1");
        var receipt = new GraphNodeKey("DeliveryNote", "DN-1");
        var invoice = new GraphNodeKey("SalesInvoice", "SI-1");
        var payment = new GraphNodeKey("Payment", "PE-1");
        var locked = new GraphNodeKey("SalesInvoice", "SI-2");
        return new GraphSnapshot(
            [
                new LineageNode(order, "Sales order 1", "Posted", "1,250.00 EUR"),
                new LineageNode(receipt, "Delivery 1", "Posted", "1,250.00 EUR"),
                new LineageNode(invoice, "Invoice 1", "Draft", "1,250.00 EUR"),
                new LineageNode(payment, "Payment 1", "Paid", "1,250.00 EUR"),
                new LineageNode(locked, string.Empty, string.Empty, string.Empty, IsLocked: true),
            ],
            [
                new LineageEdge(order, receipt, "delivers"),
                new LineageEdge(receipt, invoice, "invoices"),
                new LineageEdge(invoice, payment, "pays"),
                new LineageEdge(invoice, locked, "credit note"),
            ]);
    }
}
