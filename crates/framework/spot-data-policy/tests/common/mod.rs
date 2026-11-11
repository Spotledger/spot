//! Shared fixtures for the integration tests.
#![allow(dead_code)]

use std::collections::BTreeMap;

use spot_data_policy::{
    CivilDate, Classification, FieldClassMap, FieldValue, Record, RetentionRules,
};

/// Midnight UTC on a date, in Unix milliseconds.
pub fn at(year: i32, month: u8, day: u8) -> u64 {
    CivilDate::new(year, month, day)
        .expect("a valid date")
        .to_unix_millis()
        .expect("after 1970")
}

/// A plain record with no fields, class or jurisdiction.
pub fn record(id: &str, doctype: &str, created: u64) -> Record {
    Record {
        id: id.to_owned(),
        doctype: doctype.to_owned(),
        doc_class: None,
        jurisdiction: None,
        fields: BTreeMap::new(),
        created,
        closed: None,
        events: BTreeMap::new(),
        references: Vec::new(),
    }
}

/// A record with a statutory class and jurisdiction.
pub fn classified(id: &str, created: u64, class: &str, jurisdiction: &str) -> Record {
    Record {
        id: id.to_owned(),
        doctype: "sales_invoice".to_owned(),
        doc_class: Some(class.to_owned()),
        jurisdiction: Some(jurisdiction.to_owned()),
        fields: BTreeMap::new(),
        created,
        closed: None,
        events: BTreeMap::new(),
        references: Vec::new(),
    }
}

/// A rule file with one rule per tuple `(id, doctype, trigger, period, action)`.
pub fn rules(entries: &[(&str, &str, &str, &str, &str)]) -> RetentionRules {
    let mut text = String::new();
    for (id, doctype, trigger, period, action) in entries {
        text.push_str(&format!(
            "[[rule]]\nid = \"{id}\"\ndoctype = \"{doctype}\"\ntrigger = \"{trigger}\"\nperiod = \"{period}\"\naction = \"{action}\"\n"
        ));
    }
    RetentionRules::from_toml(&text).expect("a valid rule file")
}

/// The field classification used by the anonymisation tests.
pub fn fields() -> FieldClassMap {
    FieldClassMap::from_toml(
        "[[field]]\ndoctype = \"sales_invoice\"\nname = \"customer_name\"\nclassification = \"pii\"\n\
         [[field]]\ndoctype = \"sales_invoice\"\nname = \"customer_ref\"\nclassification = \"pii\"\nledger_relevant = true\n\
         [[field]]\ndoctype = \"sales_invoice\"\nname = \"total\"\nclassification = \"internal\"\nledger_relevant = true\n\
         [[field]]\ndoctype = \"sales_invoice\"\nname = \"note\"\nclassification = \"public\"\n",
    )
    .expect("a valid field map")
}

/// An invoice with personal and ledger fields.
pub fn invoice(id: &str, created: u64) -> Record {
    let mut record = classified(id, created, "operational", "de");
    record.fields.insert(
        "customer_name".to_owned(),
        FieldValue::Text("Ada Lovelace".to_owned()),
    );
    record.fields.insert(
        "customer_ref".to_owned(),
        FieldValue::Text("CUST-7".to_owned()),
    );
    record
        .fields
        .insert("total".to_owned(), FieldValue::Money(12_345));
    record
        .fields
        .insert("note".to_owned(), FieldValue::Text("keep me".to_owned()));
    record
}

/// The field map's classification of `customer_name`, for an assertion about the metadata.
pub fn customer_name_is_pii() -> Classification {
    fields()
        .spec("sales_invoice", "customer_name")
        .expect("classified")
        .classification
}
