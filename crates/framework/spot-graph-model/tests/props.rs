//! Property tests (brief item 5): derivation is deterministic — the same
//! metadata `Vec` in any order yields an equal model with equal ids, and a
//! relation-spec walk is likewise order-independent.

use proptest::prelude::*;

use spot_graph_model::GraphModel;

mod common;

use common::{effective_of, fixtures, task_link, task_link_spec};

/// Two distinct relation specs over the same DocType: two edges, so a shuffle
/// actually reorders something.
fn two_specs() -> Vec<spot_graph_model::RelationSpec> {
    vec![task_link_spec("works_on"), task_link_spec("mentors")]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The order of the definitions never changes the derived model: any
    /// shuffle of the fixture set derives an equal model — ids included.
    #[test]
    fn a_shuffled_definition_set_derives_the_same_model(defs in Just(fixtures()).prop_shuffle()) {
        let sorted_build = GraphModel::derive(&effective_of(fixtures()));
        let shuffled_build = GraphModel::derive(&effective_of(defs));
        prop_assert_eq!(&sorted_build, &shuffled_build);
    }

    /// The order of the relation specs never changes the model: any shuffle
    /// of the specs derives an equal relations model.
    #[test]
    fn shuffled_relation_specs_derive_the_same_model(specs in Just(two_specs()).prop_shuffle()) {
        let mut defs = fixtures();
        defs.push(task_link());
        let meta = effective_of(defs);
        let sorted_build = GraphModel::derive_relations(&meta, &two_specs());
        let shuffled_build = GraphModel::derive_relations(&meta, &specs);
        prop_assert_eq!(&sorted_build, &shuffled_build);
    }
}
