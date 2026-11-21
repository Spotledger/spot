using System;
using System.Collections.Generic;
using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The property panels are generated from the server's kind schema: one row per declared field with
/// the editor the schema asks for, integer fields that refuse fractions, select fields that refuse
/// a value outside the options, list fields that split on lines, and expression fields wired to the
/// shared expression parser for diagnostics and completion.
public sealed class PropertyPanelTests
{
    [Fact]

    public void ThePanelOfAKnownKindHasOneRowPerSchemaFieldInOrder()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPropertyPanel panel = new(document, "create_delivery", FlowFixtures.Catalogue());

        Assert.Equal("Document", panel.KindName);
        Assert.False(panel.IsUnknownKind);
        Assert.Null(panel.UnknownKindNotice);
        Assert.Equal(["action", "doctype", "operation", "mapping"], panel.Fields.Select(row => row.Key).ToArray());
        Assert.Equal("Delivery Note", panel.Field("doctype")!.Text);
        Assert.Equal(FlowFieldType.Select, panel.Field("action")!.Type);
        Assert.True(panel.Field("doctype")!.Required);
    }

    [Fact]

    public void ATextEditBecomesACommandInsteadOfAChange()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPropertyPanel panel = new(document, "create_delivery", FlowFixtures.Catalogue());

        Assert.True(panel.TrySet("doctype", "Pick List", out IFlowCommand? command, out string? error));
        Assert.Null(error);
        Assert.Equal("Delivery Note", document.Node("create_delivery")!.ConfigValue("doctype")!.ToDisplayText());

        FlowEditor editor = new(document);
        Assert.True(editor.Execute(command!));
        Assert.Equal("Pick List", document.Node("create_delivery")!.ConfigValue("doctype")!.ToDisplayText());
    }

    [Fact]

    public void AFractionIsRefusedByAWholeNumberField()
    {
        FlowDocument document = FlowFixtures.Document();
        document.AddNode(new FlowNode("pause", "wait"));
        FlowPropertyPanel panel = new(document, "pause", FlowFixtures.Catalogue());

        Assert.False(panel.TrySet("seconds", "12.5", out IFlowCommand? command, out string? error));
        Assert.Null(command);
        Assert.Contains("must be a whole number", error, StringComparison.Ordinal);
        Assert.True(panel.TrySet("seconds", "12", out IFlowCommand? good, out _));
        FlowEditor editor = new(document);
        Assert.True(editor.Execute(good!));
        Assert.Equal(FlowValue.Number(12), document.Node("pause")!.ConfigValue("seconds"));
    }

    [Fact]

    public void AValueOutsideTheOptionsIsRefusedByASelectField()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPropertyPanel panel = new(document, "create_delivery", FlowFixtures.Catalogue());

        Assert.False(panel.TrySet("action", "delete", out IFlowCommand? command, out string? error));
        Assert.Null(command);
        Assert.Contains("must be one of", error, StringComparison.Ordinal);
    }

    [Fact]

    public void ARequiredFieldRefusesAnEmptyValueAndAnOptionalOneRemovesTheKey()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPropertyPanel panel = new(document, "post_delivery", FlowFixtures.Catalogue());

        Assert.False(panel.TrySet("doctype", "   ", out _, out string? required));
        Assert.Contains("required", required, StringComparison.Ordinal);

        Assert.Equal("post", document.Node("post_delivery")!.ConfigValue("operation")!.ToDisplayText());
        Assert.True(panel.TrySet("operation", string.Empty, out IFlowCommand? remove, out _));
        FlowEditor editor = new(document);
        Assert.True(editor.Execute(remove!));
        Assert.Null(document.Node("post_delivery")!.ConfigValue("operation"));
    }

    [Fact]

    public void AListFieldSplitsOnLinesAndAnUnknownKeyHasNoRow()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPropertyPanel panel = new(document, "approve", FlowFixtures.Catalogue());

        Assert.True(panel.TrySet("outcomes", "approve\n\nhold\n", out IFlowCommand? command, out _));
        FlowEditor editor = new(document);
        Assert.True(editor.Execute(command!));
        Assert.Contains("outcomes = [\"approve\", \"hold\"]", document.ToText(), StringComparison.Ordinal);
        Assert.Throws<ArgumentException>(() => panel.TrySet("colour", "red", out _, out _));
    }

    [Fact]

    public void AnExpressionFieldCompletesOverTheSchemasFieldsAndTheFunctionLibrary()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPropertyPanel panel = new(document, "create_delivery", FlowFixtures.Catalogue());
        FlowPropertyField mapping = panel.Field("mapping")!;

        IReadOnlyList<string> dots = mapping.Complete(0);
        Assert.Contains("doc.total", dots, StringComparer.Ordinal);
        Assert.Contains("doc.approved", dots, StringComparer.Ordinal);
        Assert.Contains("contains(", dots, StringComparer.Ordinal);

        IReadOnlyList<string> typed = FlowExpressionEditor.Complete("doc.ap", 6, ["approved", "total"]);
        Assert.Equal(["doc.approved"], typed);
        Assert.Empty(panel.Field("doctype")!.Complete(0));
    }

    [Fact]

    public void AnExpressionFieldReportsTheParsersDiagnosticsAndRefusesToSet()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPropertyPanel panel = new(document, "create_delivery", FlowFixtures.Catalogue());

        FlowDiagnostic diagnostic = Assert.Single(FlowExpressionEditor.Diagnose("doc.total >"));
        Assert.StartsWith("expr.", diagnostic.Code, StringComparison.Ordinal);

        Assert.False(panel.TrySet("mapping", "doc.total >", out IFlowCommand? command, out string? error));
        Assert.Null(command);
        Assert.False(string.IsNullOrEmpty(error));
        Assert.True(panel.TrySet("mapping", "doc.total > 1000", out IFlowCommand? good, out _));
        Assert.NotNull(good);
    }

    [Fact]

    public void ARequiredFieldThatIsMissingReportsADiagnostic()
    {
        FlowDocument document = new("demo", "demo");
        document.AddNode(new FlowNode("d1", "document"));
        FlowPropertyPanel panel = new(document, "d1", FlowFixtures.Catalogue());

        FlowPropertyField action = panel.Field("action")!;
        Assert.True(panel.HasDiagnostics);
        Assert.Equal("flow.required", Assert.Single(action.Diagnostics).Code);
        Assert.Equal(FlowFieldType.Expression, panel.Field("mapping")!.Type);
    }

    [Fact]

    public void AnUnknownKindStillGetsAPanelWithItsKeysKeptAsWritten()
    {
        FlowDocument document = FlowDocument.FromText(string.Join('\n', new[]
        {
            "name = \"x\"",
            "app = \"demo\"",
            "version = 1",
            "status = \"draft\"",
            "[[nodes]]",
            "id = \"future\"",
            "kind = \"send_message\"",
            "channel = \"#ops\"",
            string.Empty,
        }));

        FlowPropertyPanel panel = new(document, "future", FlowFixtures.Catalogue());

        Assert.True(panel.IsUnknownKind);
        Assert.NotNull(panel.UnknownKindNotice);
        Assert.Equal(["channel"], panel.Fields.Select(row => row.Key).ToArray());
        Assert.Equal("#ops", panel.Field("channel")!.Text);
    }
}
