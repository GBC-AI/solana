// Service to verify accounts hashes with other trusted validator nodes.
//
// Each interval, publish the snapshat hash which is the full accounts state
// hash on gossip. Monitor gossip for messages from validators in the --trusted-validators
// set and halt the node if a mismatch is detected.

use crate::cluster_info::{ClusterInfo, CFG};
use solana_runtime::snapshot_package::{
    AccountsPackage, AccountsPackageReceiver, AccountsPackageSender,
};
use solana_sdk::{clock::Slot, hash::Hash, pubkey::Pubkey};
use std::collections::{HashMap, HashSet};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
        Arc,
    },
    thread::{self, Builder, JoinHandle},
    time::Duration,
};

pub struct AccountsHashVerifier {
    t_accounts_hash_verifier: JoinHandle<()>,
}

impl AccountsHashVerifier {
    pub fn new(
        accounts_package_receiver: AccountsPackageReceiver,
        accounts_package_sender: Option<AccountsPackageSender>,
        exit: &Arc<AtomicBool>,
        cluster_info: &Arc<ClusterInfo>,
        trusted_validators: Option<HashSet<Pubkey>>,
        halt_on_trusted_validators_accounts_hash_mismatch: bool,
        fault_injection_rate_slots: u64,
        snapshot_interval_slots: u64,
    ) -> Self {
        let exit = exit.clone();
        let cluster_info = cluster_info.clone();
        let t_accounts_hash_verifier = Builder::new()
            .name("solana-accounts-hash".to_string())
            .spawn(move || {
                let mut hashes = vec![];
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    match accounts_package_receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(accounts_package) => {
                            Self::process_accounts_package(
                                accounts_package,
                                &cluster_info,
                                &trusted_validators,
                                halt_on_trusted_validators_accounts_hash_mismatch,
                                &accounts_package_sender,
                                &mut hashes,
                                &exit,
                                fault_injection_rate_slots,
                                snapshot_interval_slots,
                            );
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => (),
                    }
                }
            })
            .unwrap();
        Self {
            t_accounts_hash_verifier,
        }
    }

    fn process_accounts_package(
        accounts_package: AccountsPackage,
        cluster_info: &ClusterInfo,
        trusted_validators: &Option<HashSet<Pubkey>>,
        halt_on_trusted_validator_accounts_hash_mismatch: bool,
        accounts_package_sender: &Option<AccountsPackageSender>,
        hashes: &mut Vec<(Slot, Hash)>,
        exit: &Arc<AtomicBool>,
        fault_injection_rate_slots: u64,
        snapshot_interval_slots: u64,
    ) {
        if fault_injection_rate_slots != 0
            && accounts_package.root % fault_injection_rate_slots == 0
        {
            // For testing, publish an invalid hash to gossip.
            use rand::{thread_rng, Rng};
            use solana_sdk::hash::extend_and_hash;
            warn!("inserting fault at slot: {}", accounts_package.root);
            let rand = thread_rng().gen_range(0, 10);
            let hash = extend_and_hash(&accounts_package.hash, &[rand]);
            hashes.push((accounts_package.root, hash));
        } else {
            hashes.push((accounts_package.root, accounts_package.hash));
        }

        while hashes.len() > CFG.MAX_SNAPSHOT_HASHES {
            hashes.remove(0);
        }

        if halt_on_trusted_validator_accounts_hash_mismatch {
            let mut slot_to_hash = HashMap::new();
            for (slot, hash) in hashes.iter() {
                slot_to_hash.insert(*slot, *hash);
            }
            if Self::should_halt(&cluster_info, trusted_validators, &mut slot_to_hash) {
                exit.store(true, Ordering::Relaxed);
            }
        }

        if accounts_package.block_height % snapshot_interval_slots == 0 {
            if let Some(sender) = accounts_package_sender.as_ref() {
                if sender.send(accounts_package).is_err() {}
            }
        }

        cluster_info.push_accounts_hashes(hashes.clone());
    }

    fn should_halt(
        cluster_info: &ClusterInfo,
        trusted_validators: &Option<HashSet<Pubkey>>,
        slot_to_hash: &mut HashMap<Slot, Hash>,
    ) -> bool {
        let mut verified_count = 0;
        let mut highest_slot = 0;
        if let Some(trusted_validators) = trusted_validators.as_ref() {
            for trusted_validator in trusted_validators {
                let is_conflicting = cluster_info.get_accounts_hash_for_node(trusted_validator, |accounts_hashes|
                {
                    accounts_hashes.iter().any(|(slot, hash)| {
                        if let Some(reference_hash) = slot_to_hash.get(slot) {
                            if *hash != *reference_hash {
                                error!("Trusted validator {} produced conflicting hashes for slot: {} ({} != {})",
                                    trusted_validator,
                                    slot,
                                    hash,
                                    reference_hash,
                                );
                                true
                            } else {
                                verified_count += 1;
                                false
                            }
                        } else {
                            highest_slot = std::cmp::max(*slot, highest_slot);
                            slot_to_hash.insert(*slot, *hash);
                            false
                        }
                    })
                }).unwrap_or(false);

                if is_conflicting {
                    return true;
                }
            }
        }
        inc_new_counter_info!("accounts_hash_verifier-hashes_verified", verified_count);
        datapoint_info!(
            "accounts_hash_verifier",
            ("highest_slot_verified", highest_slot, i64),
        );
        false
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_accounts_hash_verifier.join()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_info::make_accounts_hashes_message;
    use crate::contact_info::ContactInfo;
    use solana_runtime::bank_forks::CompressionType;
    use solana_runtime::snapshot_utils::SnapshotVersion;
    use solana_sdk::{
        hash::hash,
        signature::{Keypair, Signer},
    };

    #[test]
    fn test_should_halt() {
        let keypair = Keypair::new();

        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new_with_invalid_keypair(contact_info);
        let cluster_info = Arc::new(cluster_info);

        let mut trusted_validators = HashSet::new();
        let mut slot_to_hash = HashMap::new();
        assert!(!AccountsHashVerifier::should_halt(
            &cluster_info,
            &Some(trusted_validators.clone()),
            &mut slot_to_hash,
        ));

        let validator1 = Keypair::new();
        let hash1 = hash(&[1]);
        let hash2 = hash(&[2]);
        {
            let message = make_accounts_hashes_message(&validator1, vec![(0, hash1)]).unwrap();
            cluster_info.push_message(message);
            cluster_info.flush_push_queue();
        }
        slot_to_hash.insert(0, hash2);
        trusted_validators.insert(validator1.pubkey());
        assert!(AccountsHashVerifier::should_halt(
            &cluster_info,
            &Some(trusted_validators),
            &mut slot_to_hash,
        ));
    }

    #[test]
    fn test_max_hashes() {
        solana_logger::setup();
        use std::path::PathBuf;
        use tempfile::TempDir;
        let keypair = Keypair::new();

        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new_with_invalid_keypair(contact_info);
        let cluster_info = Arc::new(cluster_info);

        let trusted_validators = HashSet::new();
        let exit = Arc::new(AtomicBool::new(false));
        let mut hashes = vec![];
        for i in 0..CFG.MAX_SNAPSHOT_HASHES + 1 {
            let snapshot_links = TempDir::new().unwrap();
            let accounts_package = AccountsPackage {
                hash: hash(&[i as u8]),
                block_height: 100 + i as u64,
                root: 100 + i as u64,
                slot_deltas: vec![],
                snapshot_links,
                tar_output_file: PathBuf::from("."),
                storages: vec![],
                compression: CompressionType::Bzip2,
                snapshot_version: SnapshotVersion::default(),
            };

            AccountsHashVerifier::process_accounts_package(
                accounts_package,
                &cluster_info,
                &Some(trusted_validators.clone()),
                false,
                &None,
                &mut hashes,
                &exit,
                0,
                100,
            );
        }
        cluster_info.flush_push_queue();
        let cluster_hashes = cluster_info
            .get_accounts_hash_for_node(&keypair.pubkey(), |c| c.clone())
            .unwrap();
        info!("{:?}", cluster_hashes);
        assert_eq!(hashes.len(), CFG.MAX_SNAPSHOT_HASHES);
        assert_eq!(cluster_hashes.len(), CFG.MAX_SNAPSHOT_HASHES);
        assert_eq!(cluster_hashes[0], (101, hash(&[1])));
        assert_eq!(
            cluster_hashes[CFG.MAX_SNAPSHOT_HASHES - 1],
            (
                100 + CFG.MAX_SNAPSHOT_HASHES as u64,
                hash(&[CFG.MAX_SNAPSHOT_HASHES as u8])
            )
        );
    }
}
