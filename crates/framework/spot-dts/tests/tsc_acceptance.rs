//! The blueprint acceptance ("scripts type-check against real fields"): when `tsc` is on
//! `PATH`, the generated file plus a correct script passes `tsc --noEmit`, and a script using a
//! missing field fails. Skipped with a printed note on a machine without Node; the test must
//! never fail for that.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

use spot_dts::generate;
use spot_meta::{Definition, FieldSpec, FieldType};

#[test]
fn generated_types_type_check_a_correct_script_and_reject_a_wrong_one() {
    let Some(tsc) = tsc_on_path() else {
        println!("tsc not on PATH; skipping the editor acceptance test");
        return;
    };
    let dir = std::env::temp_dir().join(format!("spot-dts-tsc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir created");
    let types = dir.join("spot.d.ts");
    let mut customer = Definition::new("customer");
    customer.fields = vec![
        FieldSpec::new("customer_name", FieldType::Text),
        FieldSpec::new("credit_limit", FieldType::Decimal),
    ];
    customer.fields[1].required = true;
    let generated = generate(&[customer]).unwrap();
    std::fs::write(&types, &generated.text).expect("types written");

    let good = dir.join("good.ts");
    std::fs::write(
        &good,
        r#"import type { Customer } from "./spot.d.ts";
export function check(doc: Customer): string {
  const limit: string = doc.credit_limit; // an exact decimal is a string
  return doc.customer_name ?? limit;
}
"#,
    )
    .expect("good script written");
    let bad = dir.join("bad.ts");
    std::fs::write(
        &bad,
        r#"import type { Customer } from "./spot.d.ts";
export function broken(doc: Customer): string {
  return doc.customer_nmae;
}
"#,
    )
    .expect("bad script written");

    let tsconfig = dir.join("tsconfig.json");
    std::fs::write(&tsconfig, spot_dts::tsconfig_snippet("spot.d.ts")).expect("tsconfig written");

    let for_file = |file: &PathBuf, cwd: &PathBuf| {
        Command::new(&tsc)
            .arg("--noEmit")
            .arg("--strict")
            .arg("--target")
            .arg("es2022")
            .arg("--module")
            .arg("esnext")
            .arg("--moduleResolution")
            .arg("bundler")
            .arg(file)
            .current_dir(cwd)
            .output()
            .expect("tsc ran")
    };
    let good_out = for_file(&good, &dir);
    assert!(
        good_out.status.success(),
        "the correct script must type-check: {}",
        String::from_utf8_lossy(&good_out.stderr)
    );
    let bad_out = for_file(&bad, &dir);
    assert!(
        !bad_out.status.success(),
        "a script using a missing field must NOT type-check"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `tsc` executable, when Node and TypeScript are installed.
fn tsc_on_path() -> Option<PathBuf> {
    // `which`-style probing without depending on `which` (no new dependencies).
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("tsc");
        if candidate.is_file() {
            return Some(candidate);
        }
        // npm installs `tsc` as a shell script/`.cmd`; the plain file check may miss a node
        // script without an extension next to it, so try running `tsc --version` once.
    }
    let probe = Command::new("tsc").arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => Some(PathBuf::from("tsc")),
        _ => None,
    }
}
