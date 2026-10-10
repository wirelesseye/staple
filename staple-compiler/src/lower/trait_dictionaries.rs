//! Trait dictionary prefix layout (T-D).
//!
//! Every prefix field is a pointer. Offsets are measured in pointer slots,
//! leaving target pointer width and alignment to codegen. The implementation's
//! descriptor and sub-dictionary environment starts after the fixed prefix.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraitDictionaryLayout {
    /// Trait members, including defaults, in declaration order.
    pub slots: Range<usize>,
    /// Prerequisite dictionaries in declaration order.
    pub prerequisites: Range<usize>,
    /// Associated-type descriptors in declaration order.
    pub associated: Range<usize>,
    /// First pointer slot of the implementation-specific tail.
    pub environment: usize,
}

impl TraitDictionaryLayout {
    pub(crate) fn new(methods: usize, prerequisites: usize, associated: usize) -> Self {
        let associated_start = methods + prerequisites;
        let environment = associated_start + associated;
        Self {
            slots: 0..methods,
            prerequisites: methods..associated_start,
            associated: associated_start..environment,
            environment,
        }
    }
}

/// Runtime dictionary obligations (T-B), in checked bound order.
/// Marker traits are supplied by descriptors or remain compile-time facts.
/// Checked bounds include prerequisites; this list is for recipe planning,
/// not the hidden ABI count, which uses the declared bounds (T-C).
pub(super) fn runtime_obligations(
    ids: &super::LoweredSemanticIds,
    bounds: &[crate::CheckedTraitBound],
    substitution: &std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>,
    runtime: &std::collections::BTreeSet<crate::TypeParameterId>,
) -> Vec<crate::CheckedTraitBound> {
    bounds
        .iter()
        .filter_map(|bound| {
            if [
                ids.copy_trait,
                ids.sized_trait,
                ids.drop_trait,
                ids.natural_trait,
            ]
            .contains(&Some(bound.trait_id))
            {
                return None;
            }
            let arguments = bound
                .arguments
                .iter()
                .map(|argument| crate::substitute_type(argument.clone(), substitution))
                .collect::<Vec<_>>();
            let mut referenced = std::collections::BTreeSet::new();
            for argument in &arguments {
                super::instance_resolution::collect_referenced_parameters(
                    argument,
                    &mut referenced,
                );
            }
            (!referenced.is_disjoint(runtime)).then_some(crate::CheckedTraitBound {
                trait_id: bound.trait_id,
                arguments,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_obligations_follow_substitution_and_preserve_checked_order() {
        use crate::{CheckedTraitBound, CheckedType, TraitId, TypeParameterId};
        use std::collections::{BTreeSet, HashMap};
        let parameter = |id| CheckedType::Parameter {
            id: TypeParameterId(id),
            name: format!("P{id}"),
            sized: true,
            parameter_product_capable: false,
        };
        let bound = |id, argument| CheckedTraitBound {
            trait_id: TraitId(id),
            arguments: vec![argument],
        };
        let ids = super::super::LoweredSemanticIds {
            copy_trait: Some(TraitId(1)),
            sized_trait: Some(TraitId(2)),
            drop_trait: Some(TraitId(3)),
            natural_trait: Some(TraitId(4)),
            ..Default::default()
        };
        let bounds = vec![
            bound(9, CheckedType::Ref(Box::new(parameter(0)))),
            bound(8, parameter(1)),
            bound(7, parameter(2)),
            bound(1, parameter(0)),
            bound(2, parameter(0)),
            bound(3, parameter(0)),
            bound(4, parameter(0)),
        ];
        // P0 becomes a different runtime parameter; P1 becomes concrete;
        // P2 remains irrelevant to this instance. Include a projection-like
        // high runtime ID: dictionary derivation makes no declared-ID assumption.
        let projected = TypeParameterId(1_000_000);
        let substitution = HashMap::from([
            (TypeParameterId(0), parameter(projected.0)),
            (TypeParameterId(1), CheckedType::I32),
        ]);
        let runtime = BTreeSet::from([projected]);
        assert_eq!(
            runtime_obligations(&ids, &bounds, &substitution, &runtime),
            vec![bound(9, CheckedType::Ref(Box::new(parameter(projected.0))))]
        );
        assert!(runtime_obligations(&ids, &bounds, &substitution, &BTreeSet::new()).is_empty());
        let runtime = BTreeSet::from([projected, TypeParameterId(2)]);
        let selected = runtime_obligations(&ids, &bounds, &substitution, &runtime);
        assert_eq!(
            selected
                .iter()
                .map(|bound| bound.trait_id)
                .collect::<Vec<_>>(),
            vec![TraitId(9), TraitId(7)]
        );
    }

    #[test]
    fn empty_and_multi_member_prefixes_are_contiguous() {
        let empty = TraitDictionaryLayout::new(0, 0, 0);
        assert_eq!(empty.environment, 0);
        let layout = TraitDictionaryLayout::new(3, 2, 2);
        assert_eq!(layout.slots, 0..3);
        assert_eq!(layout.prerequisites, 3..5);
        assert_eq!(layout.associated, 5..7);
        assert_eq!(layout.environment, 7);
    }
}
