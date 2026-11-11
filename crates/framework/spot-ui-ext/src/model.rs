//! The typed model of one app's `ui` document: the closed objects of the schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One app's whole contribution to the client (the contract's `bundle.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiDocument {
    /// The contract version the document is written for; `0.1.0` today.
    pub contract_version: String,
    /// The oldest shell that can render the document.
    pub requires_shell: String,
    /// The app that contributes the document.
    pub app: String,
    /// A free comment, as the corpus uses for its expected code.
    #[serde(rename = "$comment", default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// The app's actions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    /// The app's side panels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panels: Vec<Panel>,
    /// The app's composite components.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
    /// The app's home tiles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiles: Vec<Tile>,
    /// The app's document tabs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<Tab>,
}

/// A declarative action with exactly one effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    /// The namespaced id, `app.name`.
    pub id: String,
    /// `app:<name>`, the contributing app.
    pub origin: String,
    /// A `key:<id>` translation key or a literal.
    pub label: String,
    /// A token of the closed icon set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The slot the action fills.
    pub slot: String,
    /// The DocType the action sits on; required in `doc.*` and `list.*` slots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctype: Option<String>,
    /// The order hint; 0 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// Shown while this expression holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_if: Option<String>,
    /// Enabled while this expression holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_if: Option<String>,
    /// A permission that must exist; it only hides the action from users without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// A confirmation shown before the first send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    /// Call a declared server method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<Call>,
    /// Open a registered destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<DestinationRef>,
    /// Navigate to a path under one of the app's declared routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigate_url: Option<String>,
}

/// A call of a server method the app's manifest declares.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// The DocType the method belongs to.
    pub doctype: String,
    /// The method's name.
    pub method: String,
    /// Argument expressions, by argument name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, String>,
    /// What the client does after a successful call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<Then>,
}

/// What happens after a call succeeds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Then {
    /// A fixed follow-up.
    Keyword(ThenKeyword),
    /// Open a destination.
    Open {
        /// The destination to open and its filter expressions.
        open: DestinationRef,
    },
}

/// The fixed follow-ups of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThenKeyword {
    /// Reload the document.
    Refresh,
    /// Show the method's message.
    ShowMessage,
    /// Offer the method's file.
    Download,
}

/// A registered destination with filter expressions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationRef {
    /// The destination id, `app.name`.
    pub destination: String,
    /// Filter values, each an expression.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filters: BTreeMap<String, String>,
}

/// A side panel: an ordered list of blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    /// The namespaced id, `app.name`.
    pub id: String,
    /// `app:<name>`, the contributing app.
    pub origin: String,
    /// The panel's label.
    pub label: String,
    /// The slot the panel fills.
    pub slot: String,
    /// The DocType the panel sits on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctype: Option<String>,
    /// The order hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// Shown while this expression holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_if: Option<String>,
    /// A permission that must exist; it only hides the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// The panel's blocks, in order.
    #[serde(default)]
    pub blocks: Vec<Block>,
}

/// One block of a panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    /// The document's fields, in order.
    FieldList {
        /// The DocType whose fields are listed; the panel's own when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        of: Option<String>,
        /// The field names.
        fields: Vec<String>,
    },
    /// Label and expression pairs.
    KeyValue {
        /// The pairs.
        items: Vec<KeyValueItem>,
    },
    /// Restricted markdown.
    Text {
        /// The markdown source.
        markdown: String,
    },
    /// Rows of a declared rows query.
    Table {
        /// The declared rows query.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        /// The columns, at most eight.
        columns: Vec<Column>,
        /// The most rows to show, at most 50.
        max_rows: u32,
        /// Whether a row opens the document; true when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open: Option<bool>,
    },
    /// One number from a declared aggregate query.
    Stat {
        /// The declared aggregate query.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        /// The stat's label.
        label: String,
        /// How the number is formatted.
        format: StatFormat,
    },
    /// Links to registered destinations.
    ListOfLinks {
        /// The links.
        items: Vec<LinkItem>,
    },
    /// What to show when there is nothing else.
    EmptyState {
        /// The message.
        text: String,
        /// An optional icon token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icon: Option<String>,
    },
}

/// A `key_value` block's pair.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyValueItem {
    /// The label.
    pub label: String,
    /// The value expression.
    pub value: String,
}

/// One column of a `table` block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    /// The query column's name.
    pub field: String,
    /// The column's label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// How a `stat` block formats its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatFormat {
    /// A whole number.
    Int,
    /// A decimal number.
    Decimal,
    /// A percentage.
    Percent,
    /// An amount of money.
    Currency,
}

/// One link of a `list_of_links` block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkItem {
    /// The link's label.
    pub label: String,
    /// The destination and its filters.
    pub open: DestinationRef,
}

/// A composite component: typed props, named holes and a body of layout nodes and controls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    /// The namespaced id, `app.name`.
    pub id: String,
    /// `app:<name>`, the contributing app.
    pub origin: String,
    /// The component's props.
    #[serde(default)]
    pub props: Vec<Prop>,
    /// The names of the holes a `use` may fill.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<String>,
    /// The component's body.
    pub body: Node,
}

/// One typed prop of a composite component.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prop {
    /// The prop's name, read inside the body as `doc.<name>`.
    pub name: String,
    /// The prop's type.
    #[serde(rename = "type")]
    pub ty: PropType,
    /// Whether every use must supply it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// The field vocabulary a prop's type comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropType {
    /// Text.
    Text,
    /// A whole number.
    Int,
    /// An exact decimal.
    Decimal,
    /// A truth value.
    Bool,
    /// A calendar date.
    Date,
    /// An instant.
    DateTime,
    /// A list of values.
    List,
}

/// A node of a composite's body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// Children side by side.
    Row {
        /// The children, at most 100.
        children: Vec<Node>,
        /// A column count hint, 1 to 6.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<u32>,
        /// A title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Children stacked.
    Column {
        /// The children, at most 100.
        children: Vec<Node>,
        /// A column count hint, 1 to 6.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<u32>,
        /// A title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Children in a grid.
    Grid {
        /// The children, at most 100.
        children: Vec<Node>,
        /// The grid's column count, 1 to 6.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<u32>,
        /// A title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Children in a titled group.
    Group {
        /// The children, at most 100.
        children: Vec<Node>,
        /// A column count hint, 1 to 6.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<u32>,
        /// The group's title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Repeat a body over a list prop.
    Repeat {
        /// The list prop's name.
        over: String,
        /// The most repetitions, 1 to 50.
        max: u32,
        /// The repeated body.
        body: Box<Node>,
    },
    /// Render the body while a test holds.
    When {
        /// The test expression.
        test: String,
        /// The body.
        then: Box<Node>,
    },
    /// One catalog control.
    Control {
        /// The control.
        control: ControlKind,
        /// The control's text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The control's value expression.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// The action a button fires.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_click: Option<String>,
    },
    /// Use another composite component.
    Use {
        /// The component's id.
        component: String,
        /// Expression values for the component's props.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        props: BTreeMap<String, String>,
        /// Bodies for the component's named slots.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        fill: BTreeMap<String, Node>,
    },
    /// A hole filled by the using component.
    Slot {
        /// The slot's name.
        name: String,
    },
}

/// The read-only presentations of the ADR-068 catalog, plus the one button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    /// A text label.
    Label,
    /// A heading.
    Heading,
    /// A separator line.
    Separator,
    /// A small status badge.
    Badge,
    /// A progress indicator.
    Progress,
    /// A number.
    Number,
    /// An amount of money.
    Currency,
    /// A date.
    Date,
    /// A button; its only event is a declared action.
    Button,
}

/// A home tile: a composite component plus its props.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tile {
    /// The namespaced id, `app.name`.
    pub id: String,
    /// `app:<name>`, the contributing app.
    pub origin: String,
    /// The tile's label.
    pub label: String,
    /// A token of the closed icon set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The slot the tile fills.
    pub slot: String,
    /// The DocType the tile sits on, when it sits on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctype: Option<String>,
    /// The order hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// Shown while this expression holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_if: Option<String>,
    /// A permission that must exist; it only hides the tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// The component the tile renders.
    pub component: String,
    /// The component's props: literals or aggregate queries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub props: BTreeMap<String, PropValue>,
    /// The destination the tile opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<DestinationRef>,
}

/// A tile prop's value: a literal, or the result of an aggregate query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PropValue {
    /// The result of an aggregate query.
    Query {
        /// The declared aggregate query.
        query: String,
    },
    /// A literal value.
    Literal(LiteralValue),
}

/// A literal of the contract's `value` definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LiteralValue {
    /// Text, at most 200 characters.
    Text(String),
    /// A whole number.
    Int(i64),
    /// A truth value.
    Bool(bool),
    /// No value.
    Null,
}

/// A tab of a document's tab strip, showing a panel of the same app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    /// The namespaced id, `app.name`.
    pub id: String,
    /// `app:<name>`, the contributing app.
    pub origin: String,
    /// The tab's label.
    pub label: String,
    /// A token of the closed icon set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The slot the tab fills.
    pub slot: String,
    /// The DocType the tab applies to.
    pub doctype: String,
    /// The order hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// Shown while this expression holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_if: Option<String>,
    /// A permission that must exist; it only hides the tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// The panel the tab shows.
    pub panel: String,
}

impl UiDocument {
    /// Sets the app's panels and returns the document.
    #[must_use]
    pub fn with_panels(mut self, panels: Vec<Panel>) -> Self {
        self.panels = panels;
        self
    }

    /// Sets the app's components and returns the document.
    #[must_use]
    pub fn with_components(mut self, components: Vec<Component>) -> Self {
        self.components = components;
        self
    }

    /// Sets the app's tabs and returns the document.
    #[must_use]
    pub fn with_tabs(mut self, tabs: Vec<Tab>) -> Self {
        self.tabs = tabs;
        self
    }

    /// Sets the app's tiles and returns the document.
    #[must_use]
    pub fn with_tiles(mut self, tiles: Vec<Tile>) -> Self {
        self.tiles = tiles;
        self
    }
}
