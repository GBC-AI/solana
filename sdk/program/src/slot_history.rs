//!
//! slot history
//!
pub use crate::clock::Slot;
use bv::BitVec;
use bv::BitsMut;

#[repr(C)]
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SlotHistory {
    pub bits: BitVec<u64>,
    pub next_slot: Slot,
}

impl Default for SlotHistory {
    fn default() -> Self {
        let mut bits = BitVec::new_fill(false, CFG.SLOT_HISTORY_MAX_ENTRIES);
        bits.set(0, true);
        Self { bits, next_slot: 1 }
    }
}

impl std::fmt::Debug for SlotHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SlotHistory {{ slot: {} bits:", self.next_slot)?;
        for i in 0..CFG.SLOT_HISTORY_MAX_ENTRIES {
            if self.bits.get(i) {
                write!(f, "1")?;
            } else {
                write!(f, "0")?;
            }
        }
        Ok(())
    }
}

toml_config::package_config! {
    SLOT_HISTORY_MAX_ENTRIES: u64,
}

#[derive(PartialEq, Debug)]
pub enum Check {
    Future,
    TooOld,
    Found,
    NotFound,
}

impl SlotHistory {
    pub fn add(&mut self, slot: Slot) {
        if slot > self.next_slot && slot - self.next_slot >= CFG.SLOT_HISTORY_MAX_ENTRIES {
            // Wrapped past current history,
            // clear entire bitvec.
            let full_blocks = (CFG.SLOT_HISTORY_MAX_ENTRIES as usize) / 64;
            for i in 0..full_blocks {
                self.bits.set_block(i, 0);
            }
        } else {
            for skipped in self.next_slot..slot {
                self.bits.set(skipped % CFG.SLOT_HISTORY_MAX_ENTRIES, false);
            }
        }
        self.bits.set(slot % CFG.SLOT_HISTORY_MAX_ENTRIES, true);
        self.next_slot = slot + 1;
    }

    pub fn check(&self, slot: Slot) -> Check {
        if slot > self.newest() {
            Check::Future
        } else if slot < self.oldest() {
            Check::TooOld
        } else if self.bits.get(slot % CFG.SLOT_HISTORY_MAX_ENTRIES) {
            Check::Found
        } else {
            Check::NotFound
        }
    }

    pub fn oldest(&self) -> Slot {
        self.next_slot.saturating_sub(CFG.SLOT_HISTORY_MAX_ENTRIES)
    }

    pub fn newest(&self) -> Slot {
        self.next_slot - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::*;

    #[test]
    fn slot_history_test1() {
        solana_logger::setup();
        // should be divisible by 64 since the clear logic works on blocks
        assert_eq!(CFG.SLOT_HISTORY_MAX_ENTRIES % 64, 0);
        let mut slot_history = SlotHistory::default();
        info!("add 2");
        slot_history.add(2);
        assert_eq!(slot_history.check(0), Check::Found);
        assert_eq!(slot_history.check(1), Check::NotFound);
        for i in 3..CFG.SLOT_HISTORY_MAX_ENTRIES {
            assert_eq!(slot_history.check(i), Check::Future);
        }
        info!("add 20");
        slot_history.add(20);
        info!("add max_entries");
        slot_history.add(CFG.SLOT_HISTORY_MAX_ENTRIES);
        assert_eq!(slot_history.check(0), Check::TooOld);
        assert_eq!(slot_history.check(1), Check::NotFound);
        for i in &[2, 20, CFG.SLOT_HISTORY_MAX_ENTRIES] {
            assert_eq!(slot_history.check(*i), Check::Found);
        }
        for i in 3..20 {
            assert_eq!(slot_history.check(i), Check::NotFound, "i: {}", i);
        }
        for i in 21..CFG.SLOT_HISTORY_MAX_ENTRIES {
            assert_eq!(slot_history.check(i), Check::NotFound, "i: {}", i);
        }
        assert_eq!(
            slot_history.check(CFG.SLOT_HISTORY_MAX_ENTRIES + 1),
            Check::Future
        );

        info!("add max_entries + 3");
        let slot = 3 * CFG.SLOT_HISTORY_MAX_ENTRIES + 3;
        slot_history.add(slot);
        for i in &[0, 1, 2, 20, 21, CFG.SLOT_HISTORY_MAX_ENTRIES] {
            assert_eq!(slot_history.check(*i), Check::TooOld);
        }
        let start = slot - CFG.SLOT_HISTORY_MAX_ENTRIES + 1;
        let end = slot;
        for i in start..end {
            assert_eq!(slot_history.check(i), Check::NotFound, "i: {}", i);
        }
        assert_eq!(slot_history.check(slot), Check::Found);
    }

    #[test]
    fn slot_history_test_wrap() {
        solana_logger::setup();
        let mut slot_history = SlotHistory::default();
        info!("add 2");
        slot_history.add(2);
        assert_eq!(slot_history.check(0), Check::Found);
        assert_eq!(slot_history.check(1), Check::NotFound);
        for i in 3..CFG.SLOT_HISTORY_MAX_ENTRIES {
            assert_eq!(slot_history.check(i), Check::Future);
        }
        info!("add 20");
        slot_history.add(20);
        info!("add max_entries + 19");
        slot_history.add(CFG.SLOT_HISTORY_MAX_ENTRIES + 19);
        for i in 0..19 {
            assert_eq!(slot_history.check(i), Check::TooOld);
        }
        assert_eq!(
            slot_history.check(CFG.SLOT_HISTORY_MAX_ENTRIES),
            Check::NotFound
        );
        assert_eq!(slot_history.check(20), Check::Found);
        assert_eq!(
            slot_history.check(CFG.SLOT_HISTORY_MAX_ENTRIES + 19),
            Check::Found
        );
        assert_eq!(slot_history.check(20), Check::Found);
        for i in 21..CFG.SLOT_HISTORY_MAX_ENTRIES + 19 {
            assert_eq!(slot_history.check(i), Check::NotFound, "found: {}", i);
        }
        assert_eq!(
            slot_history.check(CFG.SLOT_HISTORY_MAX_ENTRIES + 20),
            Check::Future
        );
    }

    #[test]
    fn slot_history_test_same_index() {
        solana_logger::setup();
        let mut slot_history = SlotHistory::default();
        info!("add 3,4");
        slot_history.add(3);
        slot_history.add(4);
        assert_eq!(slot_history.check(1), Check::NotFound);
        assert_eq!(slot_history.check(2), Check::NotFound);
        assert_eq!(slot_history.check(3), Check::Found);
        assert_eq!(slot_history.check(4), Check::Found);
        slot_history.add(CFG.SLOT_HISTORY_MAX_ENTRIES + 5);
        assert_eq!(slot_history.check(5), Check::TooOld);
        for i in 6..CFG.SLOT_HISTORY_MAX_ENTRIES + 5 {
            assert_eq!(slot_history.check(i), Check::NotFound, "i: {}", i);
        }
        assert_eq!(
            slot_history.check(CFG.SLOT_HISTORY_MAX_ENTRIES + 5),
            Check::Found
        );
    }

    #[test]
    fn test_older_slot() {
        let mut slot_history = SlotHistory::default();
        slot_history.add(10);
        slot_history.add(5);
        assert_eq!(slot_history.check(0), Check::Found);
        assert_eq!(slot_history.check(5), Check::Found);
        // If we go backwards we reset?
        assert_eq!(slot_history.check(10), Check::Future);
        assert_eq!(slot_history.check(6), Check::Future);
        assert_eq!(slot_history.check(11), Check::Future);
    }

    #[test]
    fn test_oldest() {
        let mut slot_history = SlotHistory::default();
        assert_eq!(slot_history.oldest(), 0);
        slot_history.add(10);
        assert_eq!(slot_history.oldest(), 0);
        slot_history.add(CFG.SLOT_HISTORY_MAX_ENTRIES - 1);
        assert_eq!(slot_history.oldest(), 0);
        slot_history.add(CFG.SLOT_HISTORY_MAX_ENTRIES);
        assert_eq!(slot_history.oldest(), 1);
    }
}
