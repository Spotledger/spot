//! Exclusive XML Canonicalization 1.0 (`xml-exc-c14n#`), implemented over a `roxmltree` tree.
//!
//! SAML relies on XML signatures, and an XML signature's digest is taken over a *canonical*
//! form of an element, not over its bytes. This module produces that canonical form exactly as
//! [Exclusive XML Canonicalization Version 1.0] defines it: the subtree is serialised with
//! UTF-8, no XML declaration, empty elements expanded, namespace declarations that are not
//! visibly utilised dropped, namespaces already rendered by an output ancestor not repeated,
//! and namespace and attribute axes sorted lexicographically.
//!
//! The implementation follows the constrained (non-normative) algorithm of section 3.1 of the
//! specification: a subtree is canonicalised with every node in the node-set, so an apex node
//! inherits no `xml:` attributes and `xmlns=""` is emitted exactly when a default namespace
//! was rendered by an output ancestor and the element itself has none.
//!
//! No crate canonicalises XML well, and getting it subtly wrong breaks every signature; this
//! is therefore hand-written and proven against the specification's vectors in `tests/c14n.rs`.
//!
//! [Exclusive XML Canonicalization Version 1.0]: https://www.w3.org/TR/xml-exc-c14n/

use roxmltree::Node;

/// The XML namespace URI, whose prefix is implicit and never rendered as a declaration.
const XML_NS_URI: &str = "http://www.w3.org/XML/1998/namespace";

/// Canonicalises the subtree rooted at `apex`, optionally dropping one descendant element.
///
/// `skip` is the enveloped `ds:Signature` element when a signed element's digest is being
/// recomputed: the transform removes it before canonicalising. When `with_comments` is set,
/// comment nodes are emitted (the `#WithComments` variant of the algorithm).
#[must_use]
pub(crate) fn canonicalize_subtree(
    apex: Node<'_, '_>,
    skip: Option<Node<'_, '_>>,
    with_comments: bool,
) -> String {
    let mut output = String::new();
    let mut rendered: Vec<(String, String)> = Vec::new();
    write_element(apex, skip, with_comments, &mut rendered, &mut output);
    output
}

/// Canonicalises the document element of a well-formed XML document.
///
/// This is the public surface the conformance tests exercise against the specification's
/// vectors; the entry points that handle messages call [`canonicalize_subtree`] directly.
///
/// # Errors
///
/// The parse errors of [`crate::saml::xml::parse_document`]: a DTD or malformed XML is refused,
/// and arbitrary bytes never panic.
pub fn canonicalize(xml: &str, with_comments: bool) -> crate::error::Result<String> {
    let document = crate::saml::xml::parse_document(xml)?;
    Ok(canonicalize_subtree(
        document.root_element(),
        None,
        with_comments,
    ))
}

/// Canonicalises the subtree of the element whose `ID` attribute equals `id`.
///
/// # Errors
///
/// The parse errors of [`crate::saml::xml::parse_document`], or
/// [`crate::error::codes::SAML_ASSERTION_MISSING`] when no element carries that ID.
pub fn canonicalize_by_id(
    xml: &str,
    id: &str,
    with_comments: bool,
) -> crate::error::Result<String> {
    let document = crate::saml::xml::parse_document(xml)?;
    let element = document
        .descendants()
        .find(|node| node.attribute("ID") == Some(id))
        .ok_or_else(|| {
            crate::error::SsoError::new(
                crate::error::codes::SAML_ASSERTION_MISSING,
                "The sign-in response could not be read.",
                "No element with that id exists in the document.",
            )
        })?;
    Ok(canonicalize_subtree(element, None, with_comments))
}

/// Writes one element, its namespace axis, its attribute axis and its children.
fn write_element(
    element: Node<'_, '_>,
    skip: Option<Node<'_, '_>>,
    with_comments: bool,
    rendered: &mut Vec<(String, String)>,
    output: &mut String,
) {
    let prefix = element_prefix(element);
    let base = rendered.len();

    // The namespace axis: render a node only when it is visibly utilised (the InclusiveNamespaces
    // PrefixList is always empty here) and the same prefix/value has not already been rendered by
    // an output ancestor.
    let mut to_render: Vec<(String, String)> = Vec::new();
    for ns in element.namespaces() {
        let ns_prefix = ns.name().unwrap_or("");
        if ns_prefix == "xml" {
            continue;
        }
        let uri = ns.uri();
        if uri.is_empty() || uri == XML_NS_URI {
            continue;
        }
        if !visibly_utilized(element, prefix, ns_prefix) {
            continue;
        }
        let entry = (ns_prefix.to_owned(), uri.to_owned());
        if !rendered.contains(&entry) && !to_render.contains(&entry) {
            to_render.push(entry);
        }
    }
    // `xmlns=""`: the element has no prefix and no default namespace of its own, but an output
    // ancestor rendered a default namespace that would otherwise leak into this element.
    let has_default_node = element
        .namespaces()
        .any(|ns| ns.name().is_none() && !ns.uri().is_empty());
    if prefix.is_none()
        && !has_default_node
        && rendered.iter().any(|(p, _)| p.is_empty())
        && !to_render.iter().any(|(p, _)| p.is_empty())
    {
        to_render.push((String::new(), String::new()));
    }
    // The default namespace node sorts lexicographically least (empty local name).
    to_render.sort_by(|a, b| a.0.cmp(&b.0));

    output.push('<');
    push_qname(output, prefix, element.tag_name().name());
    for (ns_prefix, uri) in &to_render {
        if ns_prefix.is_empty() {
            output.push_str(" xmlns=\"");
        } else {
            output.push_str(" xmlns:");
            output.push_str(ns_prefix);
            output.push_str("=\"");
        }
        escape_attribute(uri, output);
        output.push('"');
        rendered.push((ns_prefix.clone(), uri.clone()));
    }

    // The attribute axis, sorted by namespace URI then local name (an empty URI is least).
    let mut attributes: Vec<(String, &str, &str, Option<&str>)> = Vec::new();
    for attr in element.attributes() {
        let ns_uri = attr.namespace().unwrap_or("");
        let attr_prefix = match attr.namespace() {
            None => None,
            Some(uri) => element.lookup_prefix(uri),
        };
        attributes.push((ns_uri.to_owned(), attr.name(), attr.value(), attr_prefix));
    }
    attributes.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    for (_, local, value, attr_prefix) in attributes {
        output.push(' ');
        push_qname(output, attr_prefix, local);
        output.push_str("=\"");
        escape_attribute(value, output);
        output.push('"');
    }
    output.push('>');

    for child in element.children() {
        if skip.is_some_and(|node| node == child) {
            continue;
        }
        if child.is_element() {
            write_element(child, skip, with_comments, rendered, output);
        } else if child.is_text() {
            escape_text(child.text().unwrap_or(""), output);
        } else if child.is_comment() {
            if with_comments {
                output.push_str("<!--");
                output.push_str(child.text().unwrap_or(""));
                output.push_str("-->");
            }
        } else if child.is_pi()
            && let Some(pi) = child.pi()
        {
            output.push_str("<?");
            output.push_str(pi.target);
            if let Some(value) = pi.value.filter(|value| !value.is_empty()) {
                output.push(' ');
                output.push_str(value);
            }
            output.push_str("?>");
        }
    }

    output.push_str("</");
    push_qname(output, prefix, element.tag_name().name());
    output.push('>');

    rendered.truncate(base);
}

/// The element's namespace prefix, or `None` when it uses the default namespace (or none).
fn element_prefix<'a, 'input>(element: Node<'a, 'input>) -> Option<&'a str> {
    match element.tag_name().namespace() {
        None => None,
        Some(uri) => element.lookup_prefix(uri),
    }
}

/// Whether `element` or one of its attributes uses `wanted_prefix` (the empty string is the
/// default namespace). Only the prefix matters: XML resolution guarantees its URI matches.
fn visibly_utilized(
    element: Node<'_, '_>,
    element_prefix: Option<&str>,
    wanted_prefix: &str,
) -> bool {
    let element_uses = match element_prefix {
        Some(prefix) => prefix == wanted_prefix,
        None => wanted_prefix.is_empty(),
    };
    if element_uses {
        return true;
    }
    element.attributes().any(|attr| {
        let attr_prefix = match attr.namespace() {
            None => None,
            Some(uri) => match uri {
                XML_NS_URI => Some("xml"),
                _ => element.lookup_prefix(uri),
            },
        };
        match attr_prefix {
            Some(prefix) => prefix == wanted_prefix,
            None => false,
        }
    })
}

/// Writes a qualified name: `prefix:local` or just `local`.
fn push_qname(output: &mut String, prefix: Option<&str>, local: &str) {
    if let Some(prefix) = prefix {
        output.push_str(prefix);
        output.push(':');
    }
    output.push_str(local);
}

/// Escapes character content: `&`, `<`, `>`, and carriage return.
fn escape_text(text: &str, output: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\r' => output.push_str("&#xD;"),
            _ => output.push(ch),
        }
    }
}

/// Escapes an attribute value: `&`, `<`, `"`, and the whitespace characters tab, newline and
/// carriage return.
fn escape_attribute(text: &str, output: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '"' => output.push_str("&quot;"),
            '\t' => output.push_str("&#x9;"),
            '\n' => output.push_str("&#xA;"),
            '\r' => output.push_str("&#xD;"),
            _ => output.push(ch),
        }
    }
}
