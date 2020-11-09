//! named accounts for synthesized data accounts for bank state, etc.
//!
//! this account carries the Bank's most recent bank hashes for some N parents
//!
pub use crate::slot_hashes::SlotHashes;

use crate::sysvar::Sysvar;

crate::declare_sysvar_id!("SysvarS1otHashes111111111111111111111111111", SlotHashes);

impl Sysvar for SlotHashes {
    // override
    fn size_of() -> usize {
        // hard-coded so that we don't have to construct an empty
        20_488 // golden, update if MAX_ENTRIES changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock::Slot, hash::Hash, slot_hashes::CFG as SLOT_HASHES_CFG};

    #[test]
    fn test_size_of() {
        assert_eq!(
            SlotHashes::size_of(),
            bincode::serialized_size(
                &(0..SLOT_HASHES_CFG.SLOT_MAX_ENTRIES)
                    .map(|slot| (slot as Slot, Hash::default()))
                    .collect::<SlotHashes>()
            )
            .unwrap() as usize
        );
    }
}
