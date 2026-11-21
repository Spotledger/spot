using System.Text.Json;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>
/// Reads one document — a resolved site bundle (<c>slots</c>) or one app's own <c>ui</c>
/// document (arrays) — into the typed model, dropping every item it cannot render and
/// reporting why. It never throws on document content: every reader tolerates a missing,
/// mistyped or hostile value and every check names a <c>uiext.*</c> code.
/// </summary>
internal sealed partial class ExtensionParser
{
    private static readonly string[] ActionProperties =
        ["id", "origin", "label", "icon", "slot", "doctype", "order", "visible_if", "enabled_if", "permission", "confirm", "call", "open", "navigate_url"];

    private static readonly string[] PanelProperties =
        ["id", "origin", "label", "slot", "doctype", "order", "visible_if", "permission", "blocks"];

    private static readonly string[] TileProperties =
        ["id", "origin", "label", "icon", "slot", "doctype", "order", "visible_if", "permission", "component", "props", "open"];

    private static readonly string[] TabProperties =
        ["id", "origin", "label", "icon", "slot", "doctype", "order", "visible_if", "permission", "panel"];

    private static readonly string[] ComponentProperties = ["id", "origin", "props", "slots", "body"];

    private static readonly string[] IdProperties = ["id", "origin", "label", "icon", "slot"];

    private readonly ExtensionFacts _facts;
    private readonly DiagnosticSink _sink;
    private readonly string _source;
    private readonly Dictionary<string, bool> _ids = new(StringComparer.Ordinal);
    private readonly Dictionary<string, int> _slotCounts = new(StringComparer.Ordinal);

    internal ExtensionParser(ExtensionFacts facts, DiagnosticSink sink, string source)
    {
        _facts = facts;
        _sink = sink;
        _source = source;
    }

    internal List<ActionSpec> Actions { get; } = [];

    internal List<PanelSpec> Panels { get; } = [];

    internal List<ComponentSpec> Components { get; } = [];

    internal List<TileSpec> Tiles { get; } = [];

    internal List<TabSpec> Tabs { get; } = [];

    internal string? App { get; private set; }

    internal string? RequiresShell { get; private set; }

    internal string? ShellContract { get; private set; }

    /// <summary>Reads the document body. The version gate ran before this point.</summary>
    internal void Parse(JsonElement root)
    {
        App = JsonRead.String(root, "app");
        RequiresShell = JsonRead.String(root, "requires_shell");
        ShellContract = JsonRead.String(root, "shell_contract");
        foreach (JsonElement element in JsonRead.Items(JsonRead.Array(root, "actions")))
        {
            ParseAction(element, null);
        }
        foreach (JsonElement element in JsonRead.Items(JsonRead.Array(root, "panels")))
        {
            ParsePanel(element, null);
        }
        foreach (JsonElement element in JsonRead.Items(JsonRead.Array(root, "components")))
        {
            ParseComponent(element);
        }
        foreach ((string _, JsonElement element) in JsonRead.Properties(JsonRead.Object(root, "components")))
        {
            ParseComponent(element);
        }
        foreach (JsonElement element in JsonRead.Items(JsonRead.Array(root, "tiles")))
        {
            ParseTile(element, null);
        }
        foreach (JsonElement element in JsonRead.Items(JsonRead.Array(root, "tabs")))
        {
            ParseTab(element, null);
        }
        ParseResolvedSlots(JsonRead.Object(root, "slots"));
        ValidateReferences();
    }

    private void ParseResolvedSlots(JsonElement? slots)
    {
        foreach ((string slotName, JsonElement slot) in JsonRead.Properties(slots))
        {
            if (slot.ValueKind != JsonValueKind.Object)
            {
                Report(slotName, ExtensionCodes.Schema, "a slot must be an object");
                continue;
            }
            foreach (JsonElement item in JsonRead.Items(JsonRead.Array(slot, "items")))
            {
                if (item.ValueKind != JsonValueKind.Object)
                {
                    Report(slotName, ExtensionCodes.Schema, "a slot item must be an object");
                    continue;
                }
                string kind = JsonRead.String(item, "kind") ?? string.Empty;
                switch (kind)
                {
                    case "action":
                        ParsePayload(item, "action", element => ParseAction(element, slotName));
                        break;
                    case "panel":
                        ParsePayload(item, "panel", element => ParsePanel(element, slotName));
                        break;
                    case "tile":
                        ParsePayload(item, "tile", element => ParseTile(element, slotName));
                        break;
                    case "tab":
                        ParsePayload(item, "tab", element => ParseTab(element, slotName));
                        break;
                    default:
                        Report(slotName, ExtensionCodes.Schema, $"unknown item kind `{kind}`");
                        break;
                }
            }
        }
    }

    private void ParsePayload(JsonElement item, string payloadName, Action<JsonElement> parse)
    {
        if (JsonRead.Object(item, payloadName) is not { } payload)
        {
            Report(payloadName, ExtensionCodes.Schema, $"a {payloadName} item carries its `{payloadName}` body");
            return;
        }
        parse(payload);
    }

    private bool ReadHeader(
        JsonElement element,
        ExtensionItemKind kind,
        string? slotHint,
        string[] allowedProperties,
        out Header header)
    {
        header = default;
        if (element.ValueKind != JsonValueKind.Object)
        {
            Report("?", ExtensionCodes.Schema, "an item must be an object");
            return false;
        }
        string? id = JsonRead.String(element, "id");
        if (RejectUnknownProperties(element, allowedProperties, id ?? "?"))
        {
            return false;
        }
        string? origin = JsonRead.String(element, "origin");
        string? label = JsonRead.String(element, "label");
        string? slot = JsonRead.String(element, "slot") ?? slotHint;
        if (!ExtensionPatterns.IsId(id))
        {
            Report(id ?? "?", ExtensionCodes.Schema, "an id is `app.name` in lower snake case");
            return false;
        }
        if (!ExtensionPatterns.IsOrigin(origin))
        {
            Report(id!, ExtensionCodes.OriginMismatch, "an item carries `origin: app:<app>`");
            return false;
        }
        string? originApp = ExtensionPatterns.AppOf(origin);
        string? namespaceApp = ExtensionPatterns.AppOfId(id);
        if (originApp is null || namespaceApp is null || !string.Equals(originApp, namespaceApp, StringComparison.Ordinal))
        {
            Report(id!, ExtensionCodes.OriginMismatch, $"{id} is not owned by {origin}");
            return false;
        }
        if (App is not null && !string.Equals(originApp, App, StringComparison.Ordinal))
        {
            Report(id!, ExtensionCodes.OriginMismatch, $"{id} is not owned by app:{App}");
            return false;
        }
        if (label is null || label.Length == 0)
        {
            Report(id!, ExtensionCodes.Schema, "an item carries a label");
            return false;
        }
        if (slot is null)
        {
            Report(id!, ExtensionCodes.UnknownSlot, "an item names a slot");
            return false;
        }
        SlotSpec? spec = ExtensionSlotCatalog.Find(slot);
        if (spec is null)
        {
            Report(id!, ExtensionCodes.UnknownSlot, $"slot {slot} does not exist (contract {ExtensionLimits.ContractVersion})");
            return false;
        }
        if (spec.Accepts != kind)
        {
            Report(id!, ExtensionCodes.SlotKindMismatch, $"{slot} accepts {KindName(spec.Accepts)}, not {KindName(kind)}");
            return false;
        }
        string? doctype = JsonRead.String(element, "doctype");
        if (spec.RequiresDoctype && !ExtensionPatterns.IsName(doctype))
        {
            Report(id!, ExtensionCodes.DoctypeRequired, $"{id} sits in {slot}: name its doctype");
            return false;
        }
        if (!ExtensionPatterns.IsTranslationKey(label) && ExtensionPatterns.LabelLength(label) > spec.LabelMax)
        {
            Report(id!, ExtensionCodes.LabelTooLong, $"label of {id} has {ExtensionPatterns.LabelLength(label)} characters; {slot} allows {spec.LabelMax}");
            return false;
        }
        if (!_ids.TryAdd(id!, true))
        {
            Report(id!, ExtensionCodes.DuplicateId, $"{id} is used twice");
            return false;
        }
        string countKey = string.Join('\u001f', slot, originApp, doctype ?? string.Empty);
        _slotCounts.TryGetValue(countKey, out int count);
        if (count >= spec.MaxPerApp)
        {
            Report(id!, ExtensionCodes.SlotLimit, $"{originApp} puts {count + 1} items in {slot}; the limit is {spec.MaxPerApp}");
            return false;
        }
        _slotCounts[countKey] = count + 1;
        int order = JsonRead.Int(element, "order") ?? 0;
        header = new Header(id!, originApp, label, slot, doctype, order, spec,
            CompileOptional(element, "visible_if", id!),
            CompileOptional(element, "enabled_if", id!),
            JsonRead.String(element, "permission"),
            JsonRead.String(element, "icon"));
        if (header.Permission is not null)
        {
            if (!ExtensionPatterns.IsPermission(header.Permission))
            {
                Report(id!, ExtensionCodes.UnknownPermission, $"permission {header.Permission} does not exist");
                return false;
            }
            if (_facts.IsKnown && !_facts.Permissions.Contains(header.Permission))
            {
                Report(id!, ExtensionCodes.UnknownPermission, $"permission {header.Permission} does not exist");
                return false;
            }
        }
        if (header.Icon is not null && !ExtensionLimits.IconTokens.Contains(header.Icon))
        {
            Report(id!, ExtensionCodes.UnknownIcon, $"icon {header.Icon} is not an icon token");
            return false;
        }
        return true;
    }

    private void ParseAction(JsonElement element, string? slotHint)
    {
        if (!ReadHeader(element, ExtensionItemKind.Action, slotHint, ActionProperties, out Header header))
        {
            return;
        }
        ActionEffect? effect = ParseEffect(element, header.Id);
        if (effect is null)
        {
            return;
        }
        Actions.Add(new ActionSpec(
            header.Id,
            header.App,
            header.Label,
            header.Icon,
            header.Slot,
            header.Doctype,
            header.Order,
            header.VisibleIf,
            header.EnabledIf,
            header.Permission,
            JsonRead.String(element, "confirm"),
            effect));
    }

    private ActionEffect? ParseEffect(JsonElement element, string id)
    {
        bool hasCall = JsonRead.Has(element, "call");
        bool hasOpen = JsonRead.Has(element, "open");
        bool hasNavigate = JsonRead.Has(element, "navigate_url");
        int effects = (hasCall ? 1 : 0) + (hasOpen ? 1 : 0) + (hasNavigate ? 1 : 0);
        if (effects != 1)
        {
            Report(id, ExtensionCodes.EffectInvalid, $"{id} must have exactly one effect");
            return null;
        }
        if (hasCall)
        {
            return ParseCall(element, id);
        }
        if (hasOpen)
        {
            return ParseOpen(JsonRead.Object(element, "open"), id, "open");
        }
        return ParseNavigate(element, id);
    }

    private CallEffect? ParseCall(JsonElement element, string id)
    {
        if (JsonRead.Object(element, "call") is not { } call)
        {
            Report(id, ExtensionCodes.Schema, "`call` must be an object");
            return null;
        }
        if (RejectUnknownProperties(call, ["doctype", "method", "args", "then"], id))
        {
            return null;
        }
        string? doctype = JsonRead.String(call, "doctype");
        string? method = JsonRead.String(call, "method");
        if (!ExtensionPatterns.IsName(doctype) || !ExtensionPatterns.IsName(method))
        {
            Report(id, ExtensionCodes.Schema, "a call names a doctype and a method");
            return null;
        }
        if (_facts.IsKnown && !_facts.DeclaresMethod(doctype!, method!))
        {
            Report(id, ExtensionCodes.UnknownMethod, $"{doctype}.{method} is not a declared method");
            return null;
        }
        IReadOnlyList<KeyValuePair<string, CompiledExpression>> arguments = ParseExpressions(JsonRead.Object(call, "args"), id, "args");
        (CallThen then, OpenEffect? thenOpen) = ParseThen(call, id);
        return new CallEffect(doctype!, method!, arguments, then, thenOpen);
    }

    private (CallThen Then, OpenEffect? ThenOpen) ParseThen(JsonElement call, string id)
    {
        if (!JsonRead.Has(call, "then"))
        {
            return (CallThen.Refresh, null);
        }
        if (JsonRead.String(call, "then") is { } text)
        {
            return text switch
            {
                "refresh" => (CallThen.Refresh, null),
                "show_message" => (CallThen.ShowMessage, null),
                "download" => (CallThen.Download, null),
                _ => ReportThenInvalid(id)
            };
        }
        if (JsonRead.Object(call, "then") is { } then && JsonRead.Object(then, "open") is { } open)
        {
            if (ParseOpen(open, id, "then.open") is OpenEffect effect)
            {
                return (CallThen.Open, effect);
            }
        }
        return ReportThenInvalid(id);
    }

    private (CallThen, OpenEffect?) ReportThenInvalid(string id)
    {
        Report(id, ExtensionCodes.Schema, "`then` is `refresh`, `show_message`, `download` or `{open: …}`");
        return (CallThen.Refresh, null);
    }

    private OpenEffect? ParseOpen(JsonElement? open, string id, string pointer)
    {
        if (open is not { } element)
        {
            Report(id, ExtensionCodes.Schema, $"{pointer} must be an object");
            return null;
        }
        if (RejectUnknownProperties(element, ["destination", "filters"], id))
        {
            return null;
        }
        string? destination = JsonRead.String(element, "destination");
        if (!ExtensionPatterns.IsId(destination))
        {
            Report(id, ExtensionCodes.Schema, $"{pointer} names a destination id");
            return null;
        }
        if (_facts.IsKnown && !_facts.Destinations.Contains(destination!))
        {
            Report(id, ExtensionCodes.UnknownDestination, $"destination {destination} does not exist");
            return null;
        }
        return new OpenEffect(destination!, ParseExpressions(JsonRead.Object(element, "filters"), id, "filters"));
    }

    private NavigateEffect? ParseNavigate(JsonElement element, string id)
    {
        string? path = JsonRead.String(element, "navigate_url");
        if (path is null)
        {
            Report(id, ExtensionCodes.Schema, "`navigate_url` must be a string");
            return null;
        }
        if (path.Length == 0 || path[0] != '/' || path.Contains("://", StringComparison.Ordinal) ||
            path.StartsWith("//", StringComparison.Ordinal) || path.Contains('\\'))
        {
            Report(id, ExtensionCodes.ExternalUrl, $"navigate_url {path} leaves the app");
            return null;
        }
        if (_facts.IsKnown && !_facts.CoversRoute(path))
        {
            Report(id, ExtensionCodes.UnknownRoute, $"no declared route covers {path}");
            return null;
        }
        return new NavigateEffect(path);
    }

    private List<KeyValuePair<string, CompiledExpression>> ParseExpressions(JsonElement? map, string id, string pointer)
    {
        List<KeyValuePair<string, CompiledExpression>> expressions = [];
        foreach ((string name, JsonElement value) in JsonRead.Properties(map))
        {
            if (!ExtensionPatterns.IsName(name) || expressions.Count >= ExtensionLimits.MaxArguments)
            {
                Report(id, ExtensionCodes.Schema, $"{pointer} has no room for `{name}`");
                continue;
            }
            if (value.ValueKind != JsonValueKind.String)
            {
                Report(id, ExtensionCodes.Schema, $"{pointer}.{name} is an expression string");
                continue;
            }
            expressions.Add(new(name, Compile(value.GetString()!, id)));
        }
        return expressions;
    }

    private void ParsePanel(JsonElement element, string? slotHint)
    {
        if (!ReadHeader(element, ExtensionItemKind.Panel, slotHint, PanelProperties, out Header header))
        {
            return;
        }
        List<PanelBlock> blocks = [];
        int index = 0;
        foreach (JsonElement block in JsonRead.Items(JsonRead.Array(element, "blocks")))
        {
            if (index++ >= ExtensionLimits.MaxPanelBlocks)
            {
                Report(header.Id, ExtensionCodes.Schema, $"a panel has at most {ExtensionLimits.MaxPanelBlocks} blocks");
                break;
            }
            if (ParseBlock(block, header.Id) is { } parsed)
            {
                blocks.Add(parsed);
            }
        }
        if (blocks.Count == 0)
        {
            Report(header.Id, ExtensionCodes.Schema, "a panel has at least one renderable block");
            return;
        }
        Panels.Add(new PanelSpec(header.Id, header.App, header.Label, header.Slot, header.Doctype,
            header.Order, header.VisibleIf, header.Permission, blocks));
    }

    private PanelBlock? ParseBlock(JsonElement block, string id)
    {
        if (block.ValueKind != JsonValueKind.Object)
        {
            Report(id, ExtensionCodes.Schema, "a block must be an object");
            return null;
        }
        string? type = JsonRead.String(block, "type");
        switch (type)
        {
            case "field_list":
                {
                    if (RejectUnknownProperties(block, ["type", "of", "fields"], id)) { return null; }
                    List<string> fields = [];
                    foreach (JsonElement field in JsonRead.Items(JsonRead.Array(block, "fields")))
                    {
                        if (field.ValueKind == JsonValueKind.String && ExtensionPatterns.IsName(field.GetString()) && fields.Count < ExtensionLimits.MaxListEntries)
                        {
                            fields.Add(field.GetString()!);
                        }
                    }
                    if (fields.Count == 0)
                    {
                        Report(id, ExtensionCodes.Schema, "a field_list names at least one field");
                        return null;
                    }
                    return new FieldListBlock(JsonRead.String(block, "of"), fields);
                }
            case "key_value":
                {
                    if (RejectUnknownProperties(block, ["type", "items"], id)) { return null; }
                    List<KeyValueItem> items = [];
                    foreach (JsonElement item in JsonRead.Items(JsonRead.Array(block, "items")))
                    {
                        if (items.Count >= ExtensionLimits.MaxListEntries)
                        {
                            break;
                        }
                        if (item.ValueKind != JsonValueKind.Object)
                        {
                            continue;
                        }
                        string? label = JsonRead.String(item, "label");
                        string? expression = JsonRead.String(item, "value");
                        if (label is null || expression is null)
                        {
                            Report(id, ExtensionCodes.Schema, "a key_value item has a label and a value expression");
                            continue;
                        }
                        items.Add(new KeyValueItem(label, Compile(expression, id)));
                    }
                    if (items.Count == 0)
                    {
                        Report(id, ExtensionCodes.Schema, "a key_value block has at least one item");
                        return null;
                    }
                    return new KeyValueBlock(items);
                }
            case "text":
                {
                    if (RejectUnknownProperties(block, ["type", "markdown"], id)) { return null; }
                    string? markdown = JsonRead.String(block, "markdown");
                    if (markdown is null)
                    {
                        Report(id, ExtensionCodes.Schema, "a text block carries markdown");
                        return null;
                    }
                    MarkdownModel model = MarkdownModel.Parse(markdown);
                    if (model.IsRefused)
                    {
                        Report(id, ExtensionCodes.RawHtml, $"{string.Join("; ", model.Violations)} (markup is not allowed)");
                        return null;
                    }
                    return new TextPanelBlock(model);
                }
            case "table":
                {
                    if (RejectUnknownProperties(block, ["type", "query", "columns", "max_rows", "open"], id)) { return null; }
                    if (ParseQuery(block, id, "table", "rows") is not { } query)
                    {
                        return null;
                    }
                    List<TableColumn> columns = [];
                    foreach (JsonElement column in JsonRead.Items(JsonRead.Array(block, "columns")))
                    {
                        if (columns.Count >= ExtensionLimits.MaxTableColumns)
                        {
                            break;
                        }
                        string? field = column.ValueKind == JsonValueKind.Object ? JsonRead.String(column, "field") : null;
                        if (ExtensionPatterns.IsName(field))
                        {
                            columns.Add(new TableColumn(field!, column.TryGetProperty("label", out JsonElement label) && label.ValueKind == JsonValueKind.String ? label.GetString() : null));
                        }
                    }
                    if (columns.Count == 0)
                    {
                        Report(id, ExtensionCodes.Schema, "a table names at least one column");
                        return null;
                    }
                    int maxRows = JsonRead.Int(block, "max_rows") ?? ExtensionLimits.MaxTableRows;
                    maxRows = Math.Clamp(maxRows, 1, ExtensionLimits.MaxTableRows);
                    return new TableBlock(query, columns, maxRows, JsonRead.Bool(block, "open") ?? true);
                }
            case "stat":
                {
                    if (RejectUnknownProperties(block, ["type", "query", "label", "format"], id)) { return null; }
                    string? label = JsonRead.String(block, "label");
                    string? format = JsonRead.String(block, "format");
                    if (label is null || format is not ("int" or "decimal" or "percent" or "currency"))
                    {
                        Report(id, ExtensionCodes.Schema, "a stat has a label and a known format");
                        return null;
                    }
                    if (ParseQuery(block, id, "stat", "aggregate") is not { } query)
                    {
                        return null;
                    }
                    return new StatBlock(query, label, format);
                }
            case "list_of_links":
                {
                    if (RejectUnknownProperties(block, ["type", "items"], id)) { return null; }
                    List<LinkItem> links = [];
                    foreach (JsonElement item in JsonRead.Items(JsonRead.Array(block, "items")))
                    {
                        if (links.Count >= ExtensionLimits.MaxPanelBlocks || item.ValueKind != JsonValueKind.Object)
                        {
                            break;
                        }
                        string? label = JsonRead.String(item, "label");
                        if (label is null || ParseOpen(JsonRead.Object(item, "open"), id, "open") is not { } open)
                        {
                            continue;
                        }
                        links.Add(new LinkItem(label, open));
                    }
                    if (links.Count == 0)
                    {
                        Report(id, ExtensionCodes.Schema, "a list_of_links block has at least one link");
                        return null;
                    }
                    return new ListOfLinksBlock(links);
                }
            case "empty_state":
                {
                    if (RejectUnknownProperties(block, ["type", "text", "icon"], id)) { return null; }
                    string? text = JsonRead.String(block, "text");
                    if (text is null)
                    {
                        Report(id, ExtensionCodes.Schema, "an empty_state carries text");
                        return null;
                    }
                    string? icon = JsonRead.String(block, "icon");
                    if (icon is not null && !ExtensionLimits.IconTokens.Contains(icon))
                    {
                        Report(id, ExtensionCodes.UnknownIcon, $"icon {icon} is not an icon token");
                        return null;
                    }
                    return new EmptyStateBlock(text, icon);
                }
            default:
                Report(id, ExtensionCodes.Schema, $"unknown block type `{type}`");
                return null;
        }
    }

    private string? ParseQuery(JsonElement block, string id, string kind, string needed)
    {
        string? query = JsonRead.String(block, "query");
        if (query is null || query.Length == 0)
        {
            Report(id, ExtensionCodes.QueryMissing, $"{kind} block: a data block names a query");
            return null;
        }
        if (!ExtensionPatterns.IsId(query))
        {
            Report(id, ExtensionCodes.Schema, $"query {query} is not an `app.name` id");
            return null;
        }
        if (!_facts.IsKnown)
        {
            return query;
        }
        string? declared = _facts.QueryKind(query);
        if (declared is null)
        {
            Report(id, ExtensionCodes.UnknownQuery, $"query {query} is not declared");
            return null;
        }
        if (!string.Equals(declared, needed, StringComparison.Ordinal))
        {
            Report(id, ExtensionCodes.QueryKind, $"{query} is {declared}; a {kind} needs {needed}");
            return null;
        }
        return query;
    }

    private void ParseTile(JsonElement element, string? slotHint)
    {
        if (!ReadHeader(element, ExtensionItemKind.Tile, slotHint, TileProperties, out Header header))
        {
            return;
        }
        string? component = JsonRead.String(element, "component");
        if (!ExtensionPatterns.IsId(component))
        {
            Report(header.Id, ExtensionCodes.Schema, "a tile names a component id");
            return;
        }
        Dictionary<string, TileProp> props = new(StringComparer.Ordinal);
        foreach ((string name, JsonElement value) in JsonRead.Properties(JsonRead.Object(element, "props")))
        {
            if (!ExtensionPatterns.IsName(name) || props.Count >= ExtensionLimits.MaxProps)
            {
                Report(header.Id, ExtensionCodes.Schema, $"a tile prop `{name}` is not allowed");
                continue;
            }
            if (value.ValueKind == JsonValueKind.Object)
            {
                string? query = JsonRead.String(value, "query");
                if (query is null || !ExtensionPatterns.IsId(query))
                {
                    Report(header.Id, ExtensionCodes.Schema, $"tile prop {name} names a query");
                    continue;
                }
                if (_facts.IsKnown && _facts.QueryKind(query) is null)
                {
                    Report(header.Id, ExtensionCodes.UnknownQuery, $"query {query} is not declared");
                    continue;
                }
                props[name] = new TileProp(null, query);
                continue;
            }
            if (TryLiteral(value) is { } literal)
            {
                props[name] = new TileProp(literal, null);
            }
            else
            {
                Report(header.Id, ExtensionCodes.Schema, $"tile prop {name} is a literal or a query");
            }
        }
        Tiles.Add(new TileSpec(header.Id, header.App, header.Label, header.Icon, header.Slot, header.Doctype,
            header.Order, header.VisibleIf, header.Permission, component!, props,
            ParseOpen(JsonRead.Object(element, "open"), header.Id, "open")));
    }

    private static ExpressionValue? TryLiteral(JsonElement value) => value.ValueKind switch
    {
        JsonValueKind.String => new TextValue(value.GetString()!),
        JsonValueKind.Number when value.TryGetInt64(out long number) => new IntValue(number),
        JsonValueKind.True => new BoolValue(true),
        JsonValueKind.False => new BoolValue(false),
        JsonValueKind.Null => ExpressionValue.Null,
        _ => null
    };

    private void ParseTab(JsonElement element, string? slotHint)
    {
        if (!ReadHeader(element, ExtensionItemKind.Tab, slotHint, TabProperties, out Header header))
        {
            return;
        }
        string? panel = JsonRead.String(element, "panel");
        if (!ExtensionPatterns.IsId(panel))
        {
            Report(header.Id, ExtensionCodes.Schema, "a tab names a panel id");
            return;
        }
        Tabs.Add(new TabSpec(header.Id, header.App, header.Label, header.Slot, header.Doctype, header.Order,
            header.VisibleIf, header.Permission, panel!));
    }

    /// <summary>Reports every property the contract does not allow. Returns true when there
    /// was one: objects are closed, so the item is refused rather than rendered with a
    /// property the client would have to ignore.</summary>
    private bool RejectUnknownProperties(JsonElement element, string[] allowed, string id)
    {
        bool refused = false;
        foreach (string name in JsonRead.Names(element))
        {
            if (!allowed.Contains(name, StringComparer.Ordinal))
            {
                Report(id, ExtensionCodes.Schema, $"`{name}` is not a property of this object (objects are closed)");
                refused = true;
            }
        }
        return refused;
    }

    private CompiledExpression? CompileOptional(JsonElement element, string name, string id)
    {
        string? source = JsonRead.String(element, name);
        return source is null ? null : Compile(source, id);
    }

    private CompiledExpression Compile(string source, string id)
    {
        CompiledExpression expression = CompiledExpression.Compile(source);
        expression.ReportParseError(_sink, id);
        return expression;
    }

    private void Report(string id, string code, string message) =>
        _sink.Report($"{_source}#{id}", code, message);

    private static string KindName(ExtensionItemKind kind) => kind switch
    {
        ExtensionItemKind.Action => "action",
        ExtensionItemKind.Panel => "panel",
        ExtensionItemKind.Tile => "tile",
        _ => "tab"
    };

    private readonly record struct Header(
        string Id,
        string App,
        string Label,
        string Slot,
        string? Doctype,
        int Order,
        SlotSpec Spec,
        CompiledExpression? VisibleIf,
        CompiledExpression? EnabledIf,
        string? Permission,
        string? Icon);
}
