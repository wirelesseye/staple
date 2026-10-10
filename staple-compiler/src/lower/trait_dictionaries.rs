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

#[cfg(test)]
mod tests {
    use super::*;

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
