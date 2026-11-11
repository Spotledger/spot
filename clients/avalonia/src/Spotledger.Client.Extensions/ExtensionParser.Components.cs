using System.Text.Json;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>The composite half of the reader: props, nodes, and the reference and size
/// checks that need every component of the document at once.</summary>
internal sealed partial class ExtensionParser
{
    private static readonly string[] LayoutProperties = ["node", "children", "columns", "title"];

    private static readonly string[] RepeatProperties = ["node", "over", "max", "body"];

    private static readonly string[] WhenProperties = ["node", "test", "then"];

    private static readonly string[] UseProperties = ["node", "component", "props", "fill"];

    private static readonly string[] ControlProperties = ["node", "control", "text", "value", "on_click"];

    private void ParseComponent(JsonElement element)
    {
        if (element.ValueKind != JsonValueKind.Object)
        {
            Report("?", ExtensionCodes.Schema, "a component must be an object");
            return;
        }
        string? id = JsonRead.String(element, "id");
        if (RejectUnknownProperties(element, ComponentProperties, id ?? "?"))
        {
            return;
        }
        string? origin = JsonRead.String(element, "origin");
        if (!ExtensionPatterns.IsId(id))
        {
            Report(id ?? "?", ExtensionCodes.Schema, "a component id is `app.name`");
            return;
        }
        string? originApp = ExtensionPatterns.AppOf(origin);
        if (originApp is null || !string.Equals(originApp, ExtensionPatterns.AppOfId(id), StringComparison.Ordinal) ||
            (App is not null && !string.Equals(originApp, App, StringComparison.Ordinal)))
        {
            Report(id!, ExtensionCodes.OriginMismatch, $"{id} is not owned by {origin}");
            return;
        }
        if (!_ids.TryAdd(id!, true))
        {
            Report(id!, ExtensionCodes.DuplicateId, $"{id} is used twice");
            return;
        }
        List<ComponentProp> props = [];
        foreach (JsonElement prop in JsonRead.Items(JsonRead.Array(element, "props")))
        {
            if (props.Count >= ExtensionLimits.MaxProps || prop.ValueKind != JsonValueKind.Object)
            {
                break;
            }
            string? name = JsonRead.String(prop, "name");
            string? type = JsonRead.String(prop, "type");
            if (!ExtensionPatterns.IsName(name) || type is not ("text" or "int" or "decimal" or "bool" or "date" or "datetime" or "list"))
            {
                Report(id!, ExtensionCodes.Schema, "a prop names a name and a known type");
                continue;
            }
            props.Add(new ComponentProp(name!, type!, JsonRead.Bool(prop, "required") ?? false));
        }
        List<string> slots = [];
        foreach (JsonElement slot in JsonRead.Items(JsonRead.Array(element, "slots")))
        {
            if (slots.Count < 8 && slot.ValueKind == JsonValueKind.String && ExtensionPatterns.IsName(slot.GetString()))
            {
                slots.Add(slot.GetString()!);
            }
        }
        if (JsonRead.Object(element, "body") is not { } body)
        {
            Report(id!, ExtensionCodes.Schema, "a component carries a body");
            return;
        }
        ComponentNode? node = ParseNode(body, id!, 0);
        if (node is null)
        {
            return;
        }
        Components.Add(new ComponentSpec(id!, originApp, props, slots, node));
    }

    private ComponentNode? ParseNode(JsonElement element, string id, int depth)
    {
        if (depth > 32)
        {
            Report(id, ExtensionCodes.Schema, "the body nests deeper than the reader follows");
            return null;
        }
        if (element.ValueKind != JsonValueKind.Object)
        {
            Report(id, ExtensionCodes.Schema, "a node must be an object");
            return null;
        }
        string? kind = JsonRead.String(element, "node");
        switch (kind)
        {
            case "row":
            case "column":
            case "grid":
            case "group":
                {
                    if (RejectUnknownProperties(element, LayoutProperties, id)) { return null; }
                    List<ComponentNode> children = [];
                    foreach (JsonElement child in JsonRead.Items(JsonRead.Array(element, "children")))
                    {
                        if (children.Count >= ExtensionLimits.MaxChildren)
                        {
                            Report(id, ExtensionCodes.Schema, $"a layout node has at most {ExtensionLimits.MaxChildren} children");
                            break;
                        }
                        if (ParseNode(child, id, depth + 1) is { } parsed)
                        {
                            children.Add(parsed);
                        }
                    }
                    int? columns = JsonRead.Int(element, "columns");
                    if (columns is not null)
                    {
                        columns = Math.Clamp(columns.Value, 1, ExtensionLimits.MaxGridColumns);
                    }
                    return new LayoutNode(kind, children, columns, JsonRead.String(element, "title"));
                }
            case "repeat":
                {
                    if (RejectUnknownProperties(element, RepeatProperties, id)) { return null; }
                    string? over = JsonRead.String(element, "over");
                    int? max = JsonRead.Int(element, "max");
                    if (!ExtensionPatterns.IsName(over) || max is null or < 1)
                    {
                        Report(id, ExtensionCodes.Schema, "a repeat names a prop and a max");
                        return null;
                    }
                    if (max > ExtensionLimits.MaxRepeat)
                    {
                        Report(id, ExtensionCodes.Schema, $"a repeat maxes at {ExtensionLimits.MaxRepeat}");
                        max = ExtensionLimits.MaxRepeat;
                    }
                    if (JsonRead.Object(element, "body") is not { } body || ParseNode(body, id, depth + 1) is not { } bodyNode)
                    {
                        Report(id, ExtensionCodes.Schema, "a repeat carries a body");
                        return null;
                    }
                    return new RepeatNode(over!, max.Value, bodyNode);
                }
            case "when":
                {
                    if (RejectUnknownProperties(element, WhenProperties, id)) { return null; }
                    string? test = JsonRead.String(element, "test");
                    if (test is null)
                    {
                        Report(id, ExtensionCodes.Schema, "a when carries a test");
                        return null;
                    }
                    CompiledExpression condition = Compile(test, id);
                    if (JsonRead.Object(element, "then") is not { } then || ParseNode(then, id, depth + 1) is not { } thenNode)
                    {
                        Report(id, ExtensionCodes.Schema, "a when carries a then body");
                        return null;
                    }
                    return new WhenNode(condition, thenNode);
                }
            case "control":
                {
                    if (RejectUnknownProperties(element, ControlProperties, id)) { return null; }
                    string? control = JsonRead.String(element, "control");
                    if (control is null || !ExtensionLimits.ComponentControls.Contains(control))
                    {
                        // A control the catalogue does not have is a placeholder, never a crash
                        // and never a blank: the renderer shows the name it could not draw.
                        Report(id, ExtensionCodes.Schema, $"`{control}` is not a catalog control");
                        return new ControlNode(control ?? "unknown", JsonRead.String(element, "text"), null, null);
                    }
                    string? valueSource = JsonRead.String(element, "value");
                    CompiledExpression? value = valueSource is null ? null : Compile(valueSource, id);
                    string? onClick = JsonRead.String(element, "on_click");
                    if (onClick is not null && !ExtensionPatterns.IsId(onClick))
                    {
                        Report(id, ExtensionCodes.UnknownAction, $"action {onClick} is not in this document");
                        return null;
                    }
                    return new ControlNode(control, JsonRead.String(element, "text"), value, onClick);
                }
            case "use":
                {
                    if (RejectUnknownProperties(element, UseProperties, id)) { return null; }
                    string? component = JsonRead.String(element, "component");
                    if (!ExtensionPatterns.IsId(component))
                    {
                        Report(id, ExtensionCodes.Schema, "a use names a component id");
                        return null;
                    }
                    Dictionary<string, ComponentNode> fill = new(StringComparer.Ordinal);
                    foreach ((string name, JsonElement body) in JsonRead.Properties(JsonRead.Object(element, "fill")))
                    {
                        if (fill.Count >= ExtensionLimits.MaxFillSlots)
                        {
                            break;
                        }
                        if (ParseNode(body, id, depth + 1) is { } filled)
                        {
                            fill[name] = filled;
                        }
                    }
                    return new UseNode(component!, ParseExpressions(JsonRead.Object(element, "props"), id, "props"), fill);
                }
            case "slot":
                {
                    if (RejectUnknownProperties(element, ["node", "name"], id)) { return null; }
                    string? name = JsonRead.String(element, "name");
                    if (!ExtensionPatterns.IsName(name))
                    {
                        Report(id, ExtensionCodes.Schema, "a slot names a name");
                        return null;
                    }
                    return new SlotNode(name!);
                }
            default:
                Report(id, ExtensionCodes.Schema, $"unknown node kind `{kind}`");
                return null;
        }
    }

    /// <summary>Checks the references that need the whole document, then the graph's cycles and sizes.</summary>
    private void ValidateReferences()
    {
        Dictionary<string, ComponentSpec> byId = new(StringComparer.Ordinal);
        foreach (ComponentSpec component in Components)
        {
            byId[component.Id] = component;
        }
        HashSet<string> panelIds = new(Panels.Select(static panel => panel.Id), StringComparer.Ordinal);
        HashSet<string> actionIds = new(Actions.Select(static action => action.Id), StringComparer.Ordinal);

        foreach (TileSpec tile in Tiles)
        {
            if (!byId.ContainsKey(tile.Component))
            {
                Report(tile.Id, ExtensionCodes.UnknownComponent, $"component {tile.Component} is not in this document");
            }
        }
        foreach (TabSpec tab in Tabs)
        {
            if (!panelIds.Contains(tab.Panel))
            {
                Report(tab.Id, ExtensionCodes.UnknownPanel, $"panel {tab.Panel} is not in this document");
            }
        }
        HashSet<string> dropped = new(StringComparer.Ordinal);
        foreach (ComponentSpec component in Components)
        {
            HashSet<string> used = new(StringComparer.Ordinal);
            CompositeStructure.Dependencies(component.Body, used);
            foreach (string reference in used)
            {
                if (!byId.ContainsKey(reference))
                {
                    Report(component.Id, ExtensionCodes.UnknownComponent, $"component {reference} is not in this document");
                    dropped.Add(component.Id);
                }
            }
            CheckActionReferences(component.Body, component.Id, actionIds, dropped);
        }
        foreach (string cyclic in CompositeStructure.Cycles(byId))
        {
            if (byId.ContainsKey(cyclic))
            {
                Report(cyclic, ExtensionCodes.ComponentCycle, $"component cycle through {cyclic}");
                dropped.Add(cyclic);
            }
        }
        foreach (ComponentSpec component in Components)
        {
            if (dropped.Contains(component.Id))
            {
                continue;
            }
            if (CompositeStructure.Measure(component.Body, byId) is not { } measured)
            {
                continue;
            }
            if (measured.Depth > ExtensionLimits.MaxComponentDepth)
            {
                Report(component.Id, ExtensionCodes.ComponentTooDeep, $"{component.Id} nests {measured.Depth} deep; the limit is {ExtensionLimits.MaxComponentDepth}");
                dropped.Add(component.Id);
            }
            else if (measured.Nodes > ExtensionLimits.MaxComponentNodes)
            {
                Report(component.Id, ExtensionCodes.ComponentTooLarge, $"{component.Id} expands to {measured.Nodes} nodes; the limit is {ExtensionLimits.MaxComponentNodes}");
                dropped.Add(component.Id);
            }
        }
        if (dropped.Count > 0)
        {
            Components.RemoveAll(component => dropped.Contains(component.Id));
            Tiles.RemoveAll(tile => dropped.Contains(tile.Component));
        }
    }

    private void CheckActionReferences(ComponentNode node, string owner, HashSet<string> actionIds, HashSet<string> dropped)
    {
        switch (node)
        {
            case LayoutNode layout:
                foreach (ComponentNode child in layout.Children)
                {
                    CheckActionReferences(child, owner, actionIds, dropped);
                }
                break;
            case RepeatNode repeat:
                CheckActionReferences(repeat.Body, owner, actionIds, dropped);
                break;
            case WhenNode whenNode:
                CheckActionReferences(whenNode.Then, owner, actionIds, dropped);
                break;
            case UseNode use:
                foreach (ComponentNode filled in use.Fill.Values)
                {
                    CheckActionReferences(filled, owner, actionIds, dropped);
                }
                break;
            case ControlNode { OnClick: { } onClick }:
                if (!actionIds.Contains(onClick))
                {
                    Report(owner, ExtensionCodes.UnknownAction, $"action {onClick} is not in this document");
                    dropped.Add(owner);
                }
                break;
            default:
                break;
        }
    }
}
