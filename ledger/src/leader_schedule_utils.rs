use crate::leader_schedule::LeaderSchedule;
use crate::staking_utils;
use solana_runtime::bank::Bank;
use solana_sdk::{
    clock::{Epoch, Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
    pubkey::Pubkey,
};

/// Return the leader schedule for the given epoch.
pub fn leader_schedule(epoch: Epoch, bank: &Bank) -> Option<LeaderSchedule> {
    staking_utils::staked_nodes_at_epoch(bank, epoch).map(|stakes| {
        let mut seed = [0u8; 32];
        seed[0..8].copy_from_slice(&epoch.to_le_bytes());
        let mut stakes: Vec<_> = stakes.into_iter().collect();
        sort_stakes(&mut stakes);
        LeaderSchedule::new(
            &stakes,
            seed,
            bank.get_slots_in_epoch(epoch),
            NUM_CONSECUTIVE_LEADER_SLOTS,
        )
    })
}

/// Return the leader for the given slot.
pub fn slot_leader_at(slot: Slot, bank: &Bank) -> Option<Pubkey> {
    let (epoch, slot_index) = bank.get_epoch_and_slot_index(slot);

    leader_schedule(epoch, bank).map(|leader_schedule| leader_schedule[slot_index])
}

// Returns the number of ticks remaining from the specified tick_height to the end of the
// slot implied by the tick_height
pub fn num_ticks_left_in_slot(bank: &Bank, tick_height: u64) -> u64 {
    bank.ticks_per_slot() - tick_height % bank.ticks_per_slot()
}

fn sort_stakes(stakes: &mut Vec<(Pubkey, u64)>) {
    // Sort first by stake. If stakes are the same, sort by pubkey to ensure a
    // deterministic result.
    // Note: Use unstable sort, because we dedup right after to remove the equal elements.
    stakes.sort_unstable_by(|(l_pubkey, l_stake), (r_pubkey, r_stake)| {
        if r_stake == l_stake {
            r_pubkey.cmp(&l_pubkey)
        } else {
            r_stake.cmp(&l_stake)
        }
    });

    // Now that it's sorted, we can do an O(n) dedup.
    stakes.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_runtime::genesis_utils::{create_genesis_config_with_leader, CFG as GENESIS_CFG};

    #[test]
    fn test_leader_schedule_via_bank() {
        let pubkey = solana_sdk::pubkey::new_rand();
        let genesis_config =
            create_genesis_config_with_leader(0, &pubkey, GENESIS_CFG.BOOTSTRAP_VALIDATOR_LAMPORTS)
                .genesis_config;
        let bank = Bank::new(&genesis_config);

        let pubkeys_and_stakes: Vec<_> = staking_utils::staked_nodes(&bank).into_iter().collect();
        let seed = [0u8; 32];
        let leader_schedule = LeaderSchedule::new(
            &pubkeys_and_stakes,
            seed,
            genesis_config.epoch_schedule.slots_per_epoch,
            NUM_CONSECUTIVE_LEADER_SLOTS,
        );

        assert_eq!(leader_schedule[0], pubkey);
        assert_eq!(leader_schedule[1], pubkey);
        assert_eq!(leader_schedule[2], pubkey);
    }

    #[test]
    fn test_leader_scheduler1_basic() {
        let pubkey = solana_sdk::pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            GENESIS_CFG.BOOTSTRAP_VALIDATOR_LAMPORTS,
            &pubkey,
            GENESIS_CFG.BOOTSTRAP_VALIDATOR_LAMPORTS,
        )
        .genesis_config;
        let bank = Bank::new(&genesis_config);
        assert_eq!(slot_leader_at(bank.slot(), &bank).unwrap(), pubkey);
    }

    #[test]
    fn test_sort_stakes_basic() {
        let pubkey0 = solana_sdk::pubkey::new_rand();
        let pubkey1 = solana_sdk::pubkey::new_rand();
        let mut stakes = vec![(pubkey0, 1), (pubkey1, 2)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(pubkey1, 2), (pubkey0, 1)]);
    }

    #[test]
    fn test_sort_stakes_with_dup() {
        let pubkey0 = solana_sdk::pubkey::new_rand();
        let pubkey1 = solana_sdk::pubkey::new_rand();
        let mut stakes = vec![(pubkey0, 1), (pubkey1, 2), (pubkey0, 1)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(pubkey1, 2), (pubkey0, 1)]);
    }

    #[test]
    fn test_sort_stakes_with_equal_stakes() {
        let pubkey0 = Pubkey::default();
        let pubkey1 = solana_sdk::pubkey::new_rand();
        let mut stakes = vec![(pubkey0, 1), (pubkey1, 1)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(pubkey1, 1), (pubkey0, 1)]);
    }
}
