//! Source-map spot checks: an emitted token must map back to the original
//! line and column the author wrote, using oxc's own sourcemap decoder.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_ts_transpile::Options;
use spot_ts_transpile::transpile;

/// The 0-based (line, column) of the first occurrence of `needle` in `text`.
fn first_position(text: &str, needle: &str) -> (u32, u32) {
    let offset = text.find(needle).expect("needle present") as u32;
    let before = &text[..offset as usize];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map(|at| at as u32 + 1).unwrap_or(0);
    (line, offset - line_start)
}

fn assert_maps_back(source: &str, js_needle: &str) {
    let out = transpile(source, "map.ts", &Options::default()).expect("accepted");
    let json = out.source_map.clone();
    let map = oxc_sourcemap::SourceMap::from_json_string(&json).expect("map decodes");
    let (js_line, js_col) = first_position(&out.js, js_needle);
    let table = map.generate_lookup_table();
    let token = map
        .lookup_token(&table, js_line, js_col)
        .expect("the needle's position has a mapping");
    let (original_line, original_col) = first_position(source, js_needle);
    assert_eq!(
        (token.get_src_line(), token.get_src_col()),
        (original_line, original_col),
        "the {js_needle:?} token in the output must map to the author's position"
    );
}

#[test]
fn an_emitted_token_maps_to_its_original_position() {
    // The generator keeps `charge` verbatim at the top level of the method.
    assert_maps_back(
        "export class Rate {\n  charge(units: number): number {\n    return units * 3;\n  }\n}\n",
        "charge",
    );
}

#[test]
fn a_function_declaration_maps_to_its_original_position() {
    assert_maps_back(
        "export function pick(id: string): string {\n  return id;\n}\n",
        "export function pick",
    );
}

#[test]
fn an_enum_member_expression_maps_to_its_original_position() {
    // `Size.Large` survives in the output of a non-const enum.
    assert_maps_back(
        "enum Size { Medium = 4, Large = 8 }\n\nexport const area = (size: Size): number => size === Size.Large ? 64 : 16;\n",
        "Size.Large",
    );
}

#[test]
fn stripped_types_leave_the_remaining_statements_mapped() {
    // The `return total;` line's `total` identifier maps back despite the
    // interface above it disappearing from the output.
    assert_maps_back(
        "interface Opts { total?: number }\n\nexport const read = (opts: Opts): number => opts.total ?? 0;\n",
        "opts.total ?? 0",
    );
}
