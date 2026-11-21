//! The bridge from a panel's `field_list` to the view vocabulary of `spot_views::Item`.

use spot_views::Item;

use crate::model::Block;

/// The view items a `field_list` block names, in order.
///
/// Returns `None` for every other block: only a field list shares the view vocabulary.
#[must_use]
pub fn field_list_items(block: &Block) -> Option<Vec<Item>> {
    let Block::FieldList { fields, .. } = block else {
        return None;
    };
    Some(
        fields
            .iter()
            .map(|field| Item::Field {
                field: field.clone(),
                label: None,
                control: None,
                lookup: None,
            })
            .collect(),
    )
}
