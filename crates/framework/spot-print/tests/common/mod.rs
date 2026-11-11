//! Synthetic fixture data and helpers shared by the integration tests.
//! No business data; all amounts are exact decimals (ADR-12).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use lopdf::Object;

use rust_decimal::Decimal;
use spot_print::{AssetMap, FontSet, PdfRenderer, RenderLimits, RenderOptions};
use spot_template::{Context, CtxValue, MemorySource, TemplateEngine};
use time::OffsetDateTime;

pub const INVOICE: &str = include_str!("../templates/invoice.typ.j2");
pub const RECEIPT: &str = include_str!("../templates/receipt.typ.j2");
pub const REPORT: &str = include_str!("../templates/report.typ.j2");

/// An engine holding the three reference templates plus `extra` ones.
pub fn engine(extra: &[(&str, &str)]) -> Arc<TemplateEngine<MemorySource>> {
    let mut templates = vec![
        ("invoice.typ", INVOICE),
        ("receipt.typ", RECEIPT),
        ("report.typ", REPORT),
    ];
    templates.extend_from_slice(extra);
    Arc::new(TemplateEngine::memory(&templates))
}

pub fn renderer(extra: &[(&str, &str)], limits: RenderLimits) -> PdfRenderer<MemorySource> {
    PdfRenderer::new(engine(extra), FontSet::bundled(), limits)
}

pub fn assets() -> AssetMap {
    AssetMap::new()
        .with("logo.svg", include_bytes!("../assets/logo.svg").to_vec())
        .with("qr.svg", include_bytes!("../assets/qr.svg").to_vec())
}

/// A fixed point in time: 2026-09-26 12:00:00 UTC.
pub fn opts() -> RenderOptions {
    match OffsetDateTime::from_unix_timestamp(1_790_337_600) {
        Ok(created_at) => RenderOptions::new(created_at),
        Err(error) => panic!("{error}"),
    }
}

pub fn map(pairs: Vec<(&str, CtxValue)>) -> CtxValue {
    CtxValue::Map(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn s(text: &str) -> CtxValue {
    CtxValue::from(text)
}

fn dec(units: i64) -> CtxValue {
    CtxValue::from(Decimal::new(units * 100, 2))
}

pub fn invoice() -> Context {
    let (mut sv, mut st) = (0i64, 0i64);
    let rows: Vec<CtxValue> = (1..=25i64)
        .map(|i| {
            let (qty, rate) = (i % 4 + 1, 1_000 + 50 * i);
            let value = qty * rate;
            let tax = value * 18 / 100; // exact: value is a multiple of 50
            sv += value;
            st += tax;
            map(vec![
                ("no", CtxValue::Int(i)),
                (
                    "desc",
                    s(&format!(
                        "Deformed steel bar Grade 60, {} mm",
                        8 + i % 5 * 2
                    )),
                ),
                (
                    "detail",
                    s(&format!(
                        "Batch B-{i:04}; mill test certificate MTC-{:05}",
                        10_000 + i
                    )),
                ),
                ("hs", s("7214.2000")),
                ("qty", CtxValue::Int(qty)),
                ("rate", dec(rate)),
                ("value", dec(value)),
                ("tax", dec(tax)),
                ("total", dec(value + tax)),
            ])
        })
        .collect();
    Context::new()
        .insert("title", "Sales tax invoice TEST-0001")
        .insert(
            "seller",
            map(vec![
                ("name", s("Synthetic Steel Traders (Pvt) Ltd")),
                ("address", s("Plot 0, Test Industrial Area, Lahore")),
                ("ntn", s("0000000-0")),
                ("strn", s("00-00-0000-000-00")),
            ]),
        )
        .insert(
            "buyer",
            map(vec![
                ("name", s("Synthetic Buyer & Sons")),
                ("address", s("Street 0, Test Town, Karachi")),
                ("ntn", s("1111111-1")),
                ("strn", s("11-11-1111-111-11")),
            ]),
        )
        .insert("number", "TEST-0001")
        .insert("date", "26 Sep 2026")
        .insert("rows", rows)
        .insert(
            "totals",
            map(vec![
                ("value", dec(sv)),
                ("tax", dec(st)),
                ("total", dec(sv + st)),
            ]),
        )
}

pub fn receipt() -> Context {
    let items = [
        ("دودھ", "Milk 1 L", 2, 240),
        ("چینی", "Sugar 1 kg", 1, 150),
        ("باسمتی چاول", "Basmati rice 5 kg", 1, 1_850),
        ("آٹا", "Flour 10 kg", 1, 1_300),
        ("چائے کی پتی", "Tea 475 g", 3, 1_100),
    ];
    let mut sv = 0i64;
    let rows: Vec<CtxValue> = items
        .iter()
        .map(|&(ur, en, qty, price)| {
            sv += qty * price;
            map(vec![
                ("ur", s(ur)),
                ("en", s(en)),
                ("qty", CtxValue::Int(qty)),
                ("price", dec(price)),
                ("amount", dec(qty * price)),
            ])
        })
        .collect();
    let tax = sv * 18 / 100;
    Context::new()
        .insert("title", "POS receipt TEST-R-0001")
        .insert("shop", "Synthetic Mart")
        .insert("address", "Shop 0, Test Market, Lahore")
        .insert("ntn", "0000000-0")
        .insert("number", "TEST-R-0001")
        .insert("date", "26 Sep 2026 14:05")
        .insert("rows", rows)
        .insert(
            "totals",
            map(vec![
                ("value", dec(sv)),
                ("tax", dec(tax)),
                ("total", dec(sv + tax)),
            ]),
        )
        .insert("thanks", "خریداری کا شکریہ! رسید نمبر 0001، کل 5 اشیاء")
}

/// 2,000 rows in 20 groups. Returns the context and the grand total value.
pub fn report() -> (Context, i64) {
    let warehouses = ["Lahore main", "Karachi port", "Faisalabad"];
    let (mut qty_all, mut value_all) = (0i64, 0i64);
    let groups: Vec<CtxValue> = (1..=20i64)
        .map(|g| {
            let (mut gq, mut gv) = (0i64, 0i64);
            let rows: Vec<CtxValue> = (1..=100i64)
                .map(|r| {
                    let no = (g - 1) * 100 + r;
                    let (qty, value) = (no % 7 + 1, (no % 7 + 1) * (500 + no % 13 * 25));
                    gq += qty;
                    gv += value;
                    map(vec![
                        ("no", CtxValue::Int(no)),
                        ("item", s(&format!("Synthetic product {no}"))),
                        ("warehouse", s(warehouses[(no % 3) as usize])),
                        ("qty", CtxValue::Int(qty)),
                        ("value", CtxValue::Int(value)),
                    ])
                })
                .collect();
            qty_all += gq;
            value_all += gv;
            map(vec![
                ("name", s(&format!("Group {g}"))),
                ("rows", CtxValue::List(rows)),
                ("qty", CtxValue::Int(gq)),
                ("value", CtxValue::Int(gv)),
            ])
        })
        .collect();
    let context = Context::new()
        .insert("title", "Synthetic stock valuation report")
        .insert(
            "subtitle",
            "2,000 rows in 20 groups; integer fixture values; no business data.",
        )
        .insert("groups", groups)
        .insert(
            "totals",
            map(vec![
                ("qty", CtxValue::Int(qty_all)),
                ("value", CtxValue::Int(value_all)),
            ]),
        );
    (context, value_all)
}

/// The text of every page of a parsed PDF, decoded through each font's
/// `ToUnicode` map (Typst writes CID fonts, which lopdf's own extractor
/// cannot read). Returns the page count and the text, one line per text run.
pub fn pdf_text(bytes: &[u8]) -> (usize, String) {
    let doc = match lopdf::Document::load_mem(bytes) {
        Ok(doc) => doc,
        Err(error) => panic!("PDF does not parse: {error}"),
    };
    let pages = doc.get_pages();
    let mut text = String::new();
    for page_id in pages.values() {
        let maps = font_maps(&doc, *page_id);
        let content = match doc.get_and_decode_page_content(*page_id) {
            Ok(content) => content,
            Err(error) => panic!("page content: {error}"),
        };
        let mut current: Option<&HashMap<u16, String>> = None;
        for op in &content.operations {
            match op.operator.as_str() {
                "Tf" => {
                    current = op
                        .operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| maps.get(name));
                }
                "Tj" => {
                    if let (Some(map), Some(Object::String(data, _))) =
                        (current, op.operands.first())
                    {
                        text.push_str(&decode(map, data));
                        text.push('\n');
                    }
                }
                "TJ" => {
                    if let (Some(map), Some(Object::Array(items))) = (current, op.operands.first())
                    {
                        for item in items {
                            if let Object::String(data, _) = item {
                                text.push_str(&decode(map, data));
                            }
                        }
                        text.push('\n');
                    }
                }
                _ => {}
            }
        }
    }
    (pages.len(), text)
}

fn decode(map: &HashMap<u16, String>, data: &[u8]) -> String {
    data.chunks(2)
        .filter(|pair| pair.len() == 2)
        .filter_map(|pair| map.get(&u16::from_be_bytes([pair[0], pair[1]])))
        .map(String::as_str)
        .collect()
}

fn font_maps(
    doc: &lopdf::Document,
    page: lopdf::ObjectId,
) -> HashMap<Vec<u8>, HashMap<u16, String>> {
    let mut maps = HashMap::new();
    for (name, font) in doc.get_page_fonts(page).unwrap_or_default() {
        let stream = font
            .get(b"ToUnicode")
            .and_then(|object| doc.dereference(object))
            .and_then(|(_, object)| object.as_stream());
        if let Ok(stream) = stream {
            let data = stream.decompressed_content().unwrap_or_default();
            maps.insert(name, parse_cmap(&String::from_utf8_lossy(&data)));
        }
    }
    maps
}

fn hex(token: &str) -> Option<u32> {
    u32::from_str_radix(token.trim_matches(|c| c == '<' || c == '>'), 16).ok()
}

fn utf16(token: &str) -> String {
    let digits = token.trim_matches(|c| c == '<' || c == '>');
    let units: Vec<u16> = (0..digits.len() / 4)
        .filter_map(|i| u16::from_str_radix(&digits[i * 4..i * 4 + 4], 16).ok())
        .collect();
    String::from_utf16_lossy(&units)
}

/// Parses `bfchar` and `bfrange` (single destination) sections.
fn parse_cmap(cmap: &str) -> HashMap<u16, String> {
    let mut map = HashMap::new();
    let mut mode = "";
    for line in cmap.lines().map(str::trim) {
        if line.ends_with("beginbfchar") {
            mode = "char";
        } else if line.ends_with("beginbfrange") {
            mode = "range";
        } else if line.starts_with("end") {
            mode = "";
        } else if mode == "char" {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let (Some(src), Some(dst)) = (parts.first().and_then(|t| hex(t)), parts.get(1)) {
                map.insert(src as u16, utf16(dst));
            }
        } else if mode == "range" {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let (Some(lo), Some(hi), Some(dst)) = (
                parts.first().and_then(|t| hex(t)),
                parts.get(1).and_then(|t| hex(t)),
                parts.get(2),
            ) {
                let base = hex(dst).unwrap_or(0);
                for (offset, code) in (lo..=hi).enumerate() {
                    if let Some(c) = char::from_u32(base + offset as u32) {
                        map.insert(code as u16, c.to_string());
                    }
                }
            }
        }
    }
    map
}
