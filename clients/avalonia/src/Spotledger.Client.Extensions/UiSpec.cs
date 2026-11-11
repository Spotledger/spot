using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>One item a document contributes to a slot, as the client needs it.</summary>
public interface IUiItem
{
    /// <summary>The namespaced id, <c>app.name</c>.</summary>
    string Id { get; }

    /// <summary>The app that contributes the item.</summary>
    string App { get; }

    /// <summary>The slot the item sits in.</summary>
    string Slot { get; }

    /// <summary>The DocType the item applies to, for a <c>doc.*</c> or <c>list.*</c> slot.</summary>
    string? Doctype { get; }

    /// <summary>The ordering hint, ascending.</summary>
    int Order { get; }

    /// <summary>The expression that hides the item when it is not true.</summary>
    CompiledExpression? VisibleIf { get; }

    /// <summary>The permission a viewer must hold for the item to be shown at all.</summary>
    string? Permission { get; }
}

/// <summary>The one effect an action has.</summary>
public abstract record ActionEffect;

/// <summary>The outcomes a <c>call</c> may ask for after the server answered.</summary>
public enum CallThen
{
    /// <summary>Reload the document and the panel.</summary>
    Refresh,
    /// <summary>Show the server's message.</summary>
    ShowMessage,
    /// <summary>Start the download the answer names.</summary>
    Download,
    /// <summary>Navigate to a registered destination with filters.</summary>
    Open
}

/// <summary><c>call</c>: invoke a declared server method with expression arguments.</summary>
/// <param name="Doctype">The DocType the method belongs to.</param>
/// <param name="Method">The declared method name.</param>
/// <param name="Arguments">The argument expressions, in document order.</param>
/// <param name="Then">What to do after the answer.</param>
/// <param name="ThenOpen">The destination of a <c>open</c> outcome, when <paramref name="Then"/> is <see cref="CallThen.Open"/>.</param>
public sealed record CallEffect(
    string Doctype,
    string Method,
    IReadOnlyList<KeyValuePair<string, CompiledExpression>> Arguments,
    CallThen Then,
    OpenEffect? ThenOpen = null) : ActionEffect;

/// <summary><c>open</c>: navigate to a registered destination with expression filters.</summary>
/// <param name="Destination">The destination id the registry knows.</param>
/// <param name="Filters">The filter expressions, in document order.</param>
public sealed record OpenEffect(string Destination, IReadOnlyList<KeyValuePair<string, CompiledExpression>> Filters) : ActionEffect;

/// <summary><c>navigate_url</c>: a path under one of the app's own declared routes.</summary>
/// <param name="Path">The absolute path the manifest's routes cover.</param>
public sealed record NavigateEffect(string Path) : ActionEffect;

/// <summary>An action: a label, an icon, visibility and enabled state, and exactly one effect.</summary>
/// <param name="Id">The namespaced id.</param>
/// <param name="App">The contributing app.</param>
/// <param name="Label">A literal label or a <c>key:</c> translation key.</param>
/// <param name="Icon">An icon token, or null.</param>
/// <param name="Slot">The slot the action sits in.</param>
/// <param name="Doctype">The DocType the action applies to, when the slot is DocType-scoped.</param>
/// <param name="Order">The ordering hint.</param>
/// <param name="VisibleIf">The visibility expression, or null for always visible.</param>
/// <param name="EnabledIf">The enabled expression, or null for always enabled.</param>
/// <param name="Permission">The permission a viewer must hold to see the action.</param>
/// <param name="Confirm">The text to confirm before the first send, or null.</param>
/// <param name="Effect">The action's single effect.</param>
public sealed record ActionSpec(
    string Id,
    string App,
    string Label,
    string? Icon,
    string Slot,
    string? Doctype,
    int Order,
    CompiledExpression? VisibleIf,
    CompiledExpression? EnabledIf,
    string? Permission,
    string? Confirm,
    ActionEffect Effect) : IUiItem;

/// <summary>One block of a panel.</summary>
public abstract record PanelBlock;

/// <summary><c>field_list</c>: named fields of the document, in the view vocabulary's order.</summary>
/// <param name="Of">The DocType the list names, when it is not the item's own.</param>
/// <param name="Fields">The field names.</param>
public sealed record FieldListBlock(string? Of, IReadOnlyList<string> Fields) : PanelBlock;

/// <summary>One label and one expression of a <c>key_value</c> block.</summary>
/// <param name="Label">The literal label or <c>key:</c>.</param>
/// <param name="Value">The expression that produces the value.</param>
public sealed record KeyValueItem(string Label, CompiledExpression Value);

/// <summary><c>key_value</c>: labelled expression values.</summary>
/// <param name="Items">The entries.</param>
public sealed record KeyValueBlock(IReadOnlyList<KeyValueItem> Items) : PanelBlock;

/// <summary><c>text</c>: a restricted markdown subset, already parsed to an inline model.</summary>
/// <param name="Model">The parsed markdown.</param>
public sealed record TextPanelBlock(MarkdownModel Model) : PanelBlock;

/// <summary>One column of a table block.</summary>
/// <param name="Field">The row field the column shows.</param>
/// <param name="Label">An optional literal label; the field's name when absent.</param>
public sealed record TableColumn(string Field, string? Label);

/// <summary><c>table</c>: rows from a declared server query.</summary>
/// <param name="Query">The declared rows query.</param>
/// <param name="Columns">At most eight columns.</param>
/// <param name="MaxRows">The most rows the block ever holds.</param>
/// <param name="Open">True when a row links to its document.</param>
public sealed record TableBlock(string Query, IReadOnlyList<TableColumn> Columns, int MaxRows, bool Open) : PanelBlock;

/// <summary><c>stat</c>: one aggregate from a declared query, with a format.</summary>
/// <param name="Query">The declared aggregate query.</param>
/// <param name="Label">The literal label.</param>
/// <param name="Format">One of <c>int</c>, <c>decimal</c>, <c>percent</c>, <c>currency</c>.</param>
public sealed record StatBlock(string Query, string Label, string Format) : PanelBlock;

/// <summary>One link of a <c>list_of_links</c> block.</summary>
/// <param name="Label">The literal label.</param>
/// <param name="Open">The destination and its filter expressions.</param>
public sealed record LinkItem(string Label, OpenEffect Open);

/// <summary><c>list_of_links</c>: registered destinations, never external URLs.</summary>
/// <param name="Items">The links.</param>
public sealed record ListOfLinksBlock(IReadOnlyList<LinkItem> Items) : PanelBlock;

/// <summary><c>empty_state</c>: the text a panel shows when it has nothing to show.</summary>
/// <param name="Text">The literal text.</param>
/// <param name="Icon">An icon token, or null.</param>
public sealed record EmptyStateBlock(string Text, string? Icon) : PanelBlock;

/// <summary>A panel: an ordered list of blocks, at most one of each kind.</summary>
/// <param name="Id">The namespaced id.</param>
/// <param name="App">The contributing app.</param>
/// <param name="Label">The panel's label.</param>
/// <param name="Slot">The slot the panel sits in.</param>
/// <param name="Doctype">The DocType the panel applies to.</param>
/// <param name="Order">The ordering hint.</param>
/// <param name="VisibleIf">The visibility expression, or null.</param>
/// <param name="Permission">The permission a viewer must hold to see the panel.</param>
/// <param name="Blocks">The blocks, in order.</param>
public sealed record PanelSpec(
    string Id,
    string App,
    string Label,
    string Slot,
    string? Doctype,
    int Order,
    CompiledExpression? VisibleIf,
    string? Permission,
    IReadOnlyList<PanelBlock> Blocks) : IUiItem;

/// <summary>A component prop and its declared type.</summary>
/// <param name="Name">The prop name, read as <c>doc.&lt;name&gt;</c> inside the component.</param>
/// <param name="Type">The type: <c>text</c>, <c>int</c>, <c>decimal</c>, <c>bool</c>, <c>date</c>, <c>datetime</c> or <c>list</c>.</param>
/// <param name="Required">True when an instance must supply it.</param>
public sealed record ComponentProp(string Name, string Type, bool Required);

/// <summary>One node of a component body.</summary>
public abstract record ComponentNode;

/// <summary><c>row</c>, <c>column</c>, <c>grid</c> or <c>group</c>: children in a layout primitive.</summary>
/// <param name="Kind">The layout kind.</param>
/// <param name="Children">At most one hundred children.</param>
/// <param name="Columns">The grid's column count, when the kind is <c>grid</c>.</param>
/// <param name="Title">An optional title (a group's caption).</param>
public sealed record LayoutNode(string Kind, IReadOnlyList<ComponentNode> Children, int? Columns, string? Title) : ComponentNode;

/// <summary><c>repeat</c>: the body, at most <c>max</c> times.</summary>
/// <param name="Over">The prop read for the count (an int, or a list's length).</param>
/// <param name="Max">At most fifty.</param>
/// <param name="Body">The repeated body.</param>
public sealed record RepeatNode(string Over, int Max, ComponentNode Body) : ComponentNode;

/// <summary><c>when</c>: the body, only when the test is true.</summary>
/// <param name="Test">The condition expression.</param>
/// <param name="Then">The body.</param>
public sealed record WhenNode(CompiledExpression Test, ComponentNode Then) : ComponentNode;

/// <summary><c>control</c>: a catalog presentation, or the button whose only event is <c>on_click</c>.</summary>
/// <param name="Control">The control name.</param>
/// <param name="Text">The literal text (label, heading, badge, button).</param>
/// <param name="Value">The value expression (progress, number, currency, date).</param>
/// <param name="OnClick">The id of an action of the same app, for a button.</param>
public sealed record ControlNode(string Control, string? Text, CompiledExpression? Value, string? OnClick) : ComponentNode;

/// <summary><c>use</c>: expand another component with expression props and filled slots.</summary>
/// <param name="Component">The component id.</param>
/// <param name="Props">The prop expressions, by prop name.</param>
/// <param name="Fill">The slot bodies, by slot name.</param>
public sealed record UseNode(string Component, IReadOnlyList<KeyValuePair<string, CompiledExpression>> Props, IReadOnlyDictionary<string, ComponentNode> Fill) : ComponentNode;

/// <summary><c>slot</c>: a hole an instance fills.</summary>
/// <param name="Name">The declared slot name.</param>
public sealed record SlotNode(string Name) : ComponentNode;

/// <summary>A composite component: typed props, named holes and a body.</summary>
/// <param name="Id">The namespaced id.</param>
/// <param name="App">The contributing app.</param>
/// <param name="Props">The declared props, at most sixteen.</param>
/// <param name="Slots">The declared slot names, at most eight.</param>
/// <param name="Body">The body tree.</param>
public sealed record ComponentSpec(
    string Id,
    string App,
    IReadOnlyList<ComponentProp> Props,
    IReadOnlyList<string> Slots,
    ComponentNode Body);

/// <summary>A tile prop: a literal, or the id of an aggregate query.</summary>
/// <param name="Literal">The literal value, when the prop is a literal.</param>
/// <param name="Query">The aggregate query id, when the prop is a query.</param>
public sealed record TileProp(ExpressionValue? Literal, string? Query)
{
    /// <summary>True when the prop is a query.</summary>
    public bool IsQuery => Query is not null;
}

/// <summary>A home tile: a composite with props, optionally opening a destination.</summary>
/// <param name="Id">The namespaced id.</param>
/// <param name="App">The contributing app.</param>
/// <param name="Label">The tile's label.</param>
/// <param name="Icon">An icon token, or null.</param>
/// <param name="Slot">The slot, <c>home.tiles</c>.</param>
/// <param name="Doctype">The DocType, when the slot is DocType-scoped.</param>
/// <param name="Order">The ordering hint.</param>
/// <param name="VisibleIf">The visibility expression, or null.</param>
/// <param name="Permission">The permission a viewer must hold to see the tile.</param>
/// <param name="Component">The component id to expand.</param>
/// <param name="Props">The prop values, by prop name.</param>
/// <param name="Open">The destination a click opens, or null.</param>
public sealed record TileSpec(
    string Id,
    string App,
    string Label,
    string? Icon,
    string Slot,
    string? Doctype,
    int Order,
    CompiledExpression? VisibleIf,
    string? Permission,
    string Component,
    IReadOnlyDictionary<string, TileProp> Props,
    OpenEffect? Open) : IUiItem;

/// <summary>A document tab: a label and one of the app's panels.</summary>
/// <param name="Id">The namespaced id.</param>
/// <param name="App">The contributing app.</param>
/// <param name="Label">The tab's label.</param>
/// <param name="Slot">The slot, <c>doc.tab</c>.</param>
/// <param name="Doctype">The DocType the tab applies to.</param>
/// <param name="Order">The ordering hint.</param>
/// <param name="VisibleIf">The visibility expression, or null.</param>
/// <param name="Permission">The permission a viewer must hold to see the tab.</param>
/// <param name="Panel">The panel id the tab shows.</param>
public sealed record TabSpec(
    string Id,
    string App,
    string Label,
    string Slot,
    string? Doctype,
    int Order,
    CompiledExpression? VisibleIf,
    string? Permission,
    string Panel) : IUiItem;
