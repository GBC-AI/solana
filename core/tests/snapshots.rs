// Long-running bank_forks tests

macro_rules! DEFINE_SNAPSHOT_VERSION_PARAMETERIZED_TEST_FUNCTIONS {
    ($x:ident, $y:ident, $z:ident) => {
        #[allow(non_snake_case)]
        mod $z {
            use super::*;

            const SNAPSHOT_VERSION: SnapshotVersion = SnapshotVersion::$x;
            const CLUSTER_TYPE: ClusterType = ClusterType::$y;

            #[test]
            fn test_bank_forks_status_cache_snapshot_n() {
                run_test_bank_forks_status_cache_snapshot_n(SNAPSHOT_VERSION, CLUSTER_TYPE)
            }

            #[test]
            fn test_bank_forks_snapshot_n() {
                run_test_bank_forks_snapshot_n(SNAPSHOT_VERSION, CLUSTER_TYPE)
            }

            #[test]
            fn test_concurrent_snapshot_packaging() {
                run_test_concurrent_snapshot_packaging(SNAPSHOT_VERSION, CLUSTER_TYPE)
            }

            #[test]
            fn test_slots_to_snapshot() {
                run_test_slots_to_snapshot(SNAPSHOT_VERSION, CLUSTER_TYPE)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use bincode::serialize_into;
    use crossbeam_channel::unbounded;
    use fs_extra::dir::CopyOptions;
    use itertools::Itertools;
    use solana_core::{
        cluster_info::ClusterInfo, contact_info::ContactInfo,
        snapshot_packager_service::SnapshotPackagerService,
    };
    use solana_runtime::{
        accounts_background_service::SnapshotRequestHandler,
        bank::{Bank, BankSlotDelta},
        bank_forks::{BankForks, CompressionType, SnapshotConfig},
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        snapshot_utils,
        snapshot_utils::SnapshotVersion,
        status_cache::MAX_CACHE_ENTRIES,
    };
    use solana_sdk::{
        clock::Slot,
        genesis_config::{ClusterType, GenesisConfig},
        hash::hashv,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        system_transaction,
    };
    use std::{fs, path::PathBuf, sync::atomic::AtomicBool, sync::mpsc::channel, sync::Arc};
    use tempfile::TempDir;

    DEFINE_SNAPSHOT_VERSION_PARAMETERIZED_TEST_FUNCTIONS!(V1_2_0, Development, V1_2_0_Development);
    DEFINE_SNAPSHOT_VERSION_PARAMETERIZED_TEST_FUNCTIONS!(V1_2_0, Devnet, V1_2_0_Devnet);
    DEFINE_SNAPSHOT_VERSION_PARAMETERIZED_TEST_FUNCTIONS!(V1_2_0, Testnet, V1_2_0_Testnet);
    DEFINE_SNAPSHOT_VERSION_PARAMETERIZED_TEST_FUNCTIONS!(V1_2_0, MainnetBeta, V1_2_0_MainnetBeta);

    struct SnapshotTestConfig {
        accounts_dir: TempDir,
        snapshot_dir: TempDir,
        _snapshot_output_path: TempDir,
        snapshot_config: SnapshotConfig,
        bank_forks: BankForks,
        genesis_config_info: GenesisConfigInfo,
    }

    impl SnapshotTestConfig {
        fn new(
            snapshot_version: SnapshotVersion,
            cluster_type: ClusterType,
            snapshot_interval_slots: u64,
        ) -> SnapshotTestConfig {
            let accounts_dir = TempDir::new().unwrap();
            let snapshot_dir = TempDir::new().unwrap();
            let snapshot_output_path = TempDir::new().unwrap();
            let mut genesis_config_info = create_genesis_config(10_000);
            genesis_config_info.genesis_config.cluster_type = cluster_type;
            let bank0 = Bank::new_with_paths(
                &genesis_config_info.genesis_config,
                vec![accounts_dir.path().to_path_buf()],
                &[],
                None,
                None,
            );
            bank0.freeze();
            let mut bank_forks = BankForks::new(bank0);
            bank_forks.accounts_hash_interval_slots = snapshot_interval_slots;

            let snapshot_config = SnapshotConfig {
                snapshot_interval_slots,
                snapshot_package_output_path: PathBuf::from(snapshot_output_path.path()),
                snapshot_path: PathBuf::from(snapshot_dir.path()),
                compression: CompressionType::Bzip2,
                snapshot_version,
            };
            bank_forks.set_snapshot_config(Some(snapshot_config.clone()));
            SnapshotTestConfig {
                accounts_dir,
                snapshot_dir,
                _snapshot_output_path: snapshot_output_path,
                snapshot_config,
                bank_forks,
                genesis_config_info,
            }
        }
    }

    fn restore_from_snapshot(
        old_bank_forks: &BankForks,
        old_last_slot: Slot,
        old_genesis_config: &GenesisConfig,
        account_paths: &[PathBuf],
    ) {
        let (snapshot_path, snapshot_package_output_path) = old_bank_forks
            .snapshot_config
            .as_ref()
            .map(|c| (&c.snapshot_path, &c.snapshot_package_output_path))
            .unwrap();

        let old_last_bank = old_bank_forks.get(old_last_slot).unwrap();

        let deserialized_bank = snapshot_utils::bank_from_archive(
            &account_paths,
            &[],
            &old_bank_forks
                .snapshot_config
                .as_ref()
                .unwrap()
                .snapshot_path,
            snapshot_utils::get_snapshot_archive_path(
                snapshot_package_output_path,
                &(old_last_bank.slot(), old_last_bank.get_accounts_hash()),
                &CompressionType::Bzip2,
            ),
            CompressionType::Bzip2,
            old_genesis_config,
            None,
            None,
        )
        .unwrap();

        let bank = old_bank_forks
            .banks
            .get(&deserialized_bank.slot())
            .unwrap()
            .clone();
        assert_eq!(*bank, deserialized_bank);

        let slot_snapshot_paths = snapshot_utils::get_snapshot_paths(&snapshot_path);

        for p in slot_snapshot_paths {
            snapshot_utils::remove_snapshot(p.slot, &snapshot_path).unwrap();
        }
    }

    // creates banks up to "last_slot" and runs the input function `f` on each bank created
    // also marks each bank as root and generates snapshots
    // finally tries to restore from the last bank's snapshot and compares the restored bank to the
    // `last_slot` bank
    fn run_bank_forks_snapshot_n<F>(
        snapshot_version: SnapshotVersion,
        cluster_type: ClusterType,
        last_slot: Slot,
        f: F,
        set_root_interval: u64,
    ) where
        F: Fn(&mut Bank, &Keypair),
    {
        solana_logger::setup();
        // Set up snapshotting config
        let mut snapshot_test_config = SnapshotTestConfig::new(snapshot_version, cluster_type, 1);

        let bank_forks = &mut snapshot_test_config.bank_forks;
        let mint_keypair = &snapshot_test_config.genesis_config_info.mint_keypair;

        let (s, snapshot_request_receiver) = unbounded();
        let (accounts_package_sender, _r) = channel();
        let snapshot_request_sender = Some(s);
        let snapshot_request_handler = SnapshotRequestHandler {
            snapshot_config: snapshot_test_config.snapshot_config.clone(),
            snapshot_request_receiver,
            accounts_package_sender,
        };
        for slot in 0..last_slot {
            let mut bank = Bank::new_from_parent(&bank_forks[slot], &Pubkey::default(), slot + 1);
            f(&mut bank, &mint_keypair);
            let bank = bank_forks.insert(bank);
            // Set root to make sure we don't end up with too many account storage entries
            // and to allow snapshotting of bank and the purging logic on status_cache to
            // kick in
            if slot % set_root_interval == 0 || slot == last_slot - 1 {
                // set_root should send a snapshot request
                bank_forks.set_root(bank.slot(), &snapshot_request_sender, None);
                snapshot_request_handler.handle_snapshot_requests();
            }
        }

        // Generate a snapshot package for last bank
        let last_bank = bank_forks.get(last_slot).unwrap();
        let snapshot_config = &snapshot_test_config.snapshot_config;
        let snapshot_path = &snapshot_config.snapshot_path;
        let last_slot_snapshot_path = snapshot_utils::get_snapshot_paths(snapshot_path)
            .pop()
            .expect("no snapshots found in path");
        let snapshot_package = snapshot_utils::package_snapshot(
            last_bank,
            &last_slot_snapshot_path,
            snapshot_path,
            last_bank.src.slot_deltas(&last_bank.src.roots()),
            &snapshot_config.snapshot_package_output_path,
            last_bank.get_snapshot_storages(),
            CompressionType::Bzip2,
            snapshot_version,
        )
        .unwrap();
        snapshot_utils::archive_snapshot_package(&snapshot_package).unwrap();

        // Restore bank from snapshot
        let account_paths = &[snapshot_test_config.accounts_dir.path().to_path_buf()];
        let genesis_config = &snapshot_test_config.genesis_config_info.genesis_config;
        restore_from_snapshot(bank_forks, last_slot, genesis_config, account_paths);
    }

    fn run_test_bank_forks_snapshot_n(
        snapshot_version: SnapshotVersion,
        cluster_type: ClusterType,
    ) {
        // create banks up to slot 4 and create 1 new account in each bank. test that bank 4 snapshots
        // and restores correctly
        run_bank_forks_snapshot_n(
            snapshot_version,
            cluster_type,
            4,
            |bank, mint_keypair| {
                let key1 = Keypair::new().pubkey();
                let tx =
                    system_transaction::transfer(&mint_keypair, &key1, 1, bank.last_blockhash());
                assert_eq!(bank.process_transaction(&tx), Ok(()));

                let key2 = Keypair::new().pubkey();
                let tx =
                    system_transaction::transfer(&mint_keypair, &key2, 0, bank.last_blockhash());
                assert_eq!(bank.process_transaction(&tx), Ok(()));

                bank.freeze();
            },
            1,
        );
    }

    fn goto_end_of_slot(bank: &mut Bank) {
        let mut tick_hash = bank.last_blockhash();
        loop {
            tick_hash = hashv(&[&tick_hash.as_ref(), &[42]]);
            bank.register_tick(&tick_hash);
            if tick_hash == bank.last_blockhash() {
                bank.freeze();
                return;
            }
        }
    }

    fn run_test_concurrent_snapshot_packaging(
        snapshot_version: SnapshotVersion,
        cluster_type: ClusterType,
    ) {
        solana_logger::setup();

        // Set up snapshotting config
        let mut snapshot_test_config = SnapshotTestConfig::new(snapshot_version, cluster_type, 1);

        let bank_forks = &mut snapshot_test_config.bank_forks;
        let accounts_dir = &snapshot_test_config.accounts_dir;
        let snapshots_dir = &snapshot_test_config.snapshot_dir;
        let snapshot_config = &snapshot_test_config.snapshot_config;
        let snapshot_path = &snapshot_config.snapshot_path;
        let snapshot_package_output_path = &snapshot_config.snapshot_package_output_path;
        let mint_keypair = &snapshot_test_config.genesis_config_info.mint_keypair;
        let genesis_config = &snapshot_test_config.genesis_config_info.genesis_config;

        // Take snapshot of zeroth bank
        let bank0 = bank_forks.get(0).unwrap();
        let storages = bank0.get_snapshot_storages();
        snapshot_utils::add_snapshot(snapshot_path, bank0, &storages, snapshot_version).unwrap();

        // Set up snapshotting channels
        let (sender, receiver) = channel();
        let (fake_sender, _fake_receiver) = channel();

        // Create next MAX_CACHE_ENTRIES + 2 banks and snapshots. Every bank will get snapshotted
        // and the snapshot purging logic will run on every snapshot taken. This means the three
        // (including snapshot for bank0 created above) earliest snapshots will get purged by the
        // time this loop is done.

        // Also, make a saved copy of the state of the snapshot for a bank with
        // bank.slot == saved_slot, so we can use it for a correctness check later.
        let saved_snapshots_dir = TempDir::new().unwrap();
        let saved_accounts_dir = TempDir::new().unwrap();
        let saved_slot = 4;
        let mut saved_archive_path = None;

        for forks in 0..*MAX_CACHE_ENTRIES + 2 {
            let bank = Bank::new_from_parent(
                &bank_forks[forks as u64],
                &Pubkey::default(),
                (forks + 1) as u64,
            );
            let slot = bank.slot();
            let key1 = Keypair::new().pubkey();
            let tx = system_transaction::transfer(&mint_keypair, &key1, 1, genesis_config.hash());
            assert_eq!(bank.process_transaction(&tx), Ok(()));
            bank.squash();
            let accounts_hash = bank.update_accounts_hash();

            let package_sender = {
                if slot == saved_slot as u64 {
                    // Only send one package on the real sender so that the packaging service
                    // doesn't take forever to run the packaging logic on all MAX_CACHE_ENTRIES
                    // later
                    &sender
                } else {
                    &fake_sender
                }
            };

            snapshot_utils::snapshot_bank(
                &bank,
                vec![],
                &package_sender,
                &snapshot_path,
                &snapshot_package_output_path,
                snapshot_config.snapshot_version,
                &snapshot_config.compression,
            )
            .unwrap();

            bank_forks.insert(bank);
            if slot == saved_slot as u64 {
                let options = CopyOptions::new();
                fs_extra::dir::copy(accounts_dir, &saved_accounts_dir, &options).unwrap();
                let last_snapshot_path = fs::read_dir(snapshot_path)
                    .unwrap()
                    .filter_map(|entry| {
                        let e = entry.unwrap();
                        let file_path = e.path();
                        let file_name = file_path.file_name().unwrap();
                        file_name
                            .to_str()
                            .map(|s| s.parse::<u64>().ok().map(|_| file_path.clone()))
                            .unwrap_or(None)
                    })
                    .sorted()
                    .last()
                    .unwrap();
                // only save off the snapshot of this slot, we don't need the others.
                fs_extra::dir::copy(&last_snapshot_path, &saved_snapshots_dir, &options).unwrap();

                saved_archive_path = Some(snapshot_utils::get_snapshot_archive_path(
                    snapshot_package_output_path,
                    &(slot, accounts_hash),
                    &CompressionType::Bzip2,
                ));
            }
        }

        // Purge all the outdated snapshots, including the ones needed to generate the package
        // currently sitting in the channel
        snapshot_utils::purge_old_snapshots(&snapshot_path);
        assert!(snapshot_utils::get_snapshot_paths(&snapshots_dir)
            .into_iter()
            .map(|path| path.slot)
            .eq(3..=*MAX_CACHE_ENTRIES as u64 + 2));

        // Create a SnapshotPackagerService to create tarballs from all the pending
        // SnapshotPackage's on the channel. By the time this service starts, we have already
        // purged the first two snapshots, which are needed by every snapshot other than
        // the last two snapshots. However, the packaging service should still be able to
        // correctly construct the earlier snapshots because the SnapshotPackage's on the
        // channel hold hard links to these deleted snapshots. We verify this is the case below.
        let exit = Arc::new(AtomicBool::new(false));

        let cluster_info = Arc::new(ClusterInfo::new_with_invalid_keypair(ContactInfo::default()));

        let snapshot_packager_service =
            SnapshotPackagerService::new(receiver, None, &exit, &cluster_info);

        // Close the channel so that the package service will exit after reading all the
        // packages off the channel
        drop(sender);

        // Wait for service to finish
        snapshot_packager_service
            .join()
            .expect("SnapshotPackagerService exited with error");

        // Check the archive we cached the state for earlier was generated correctly

        // before we compare, stick an empty status_cache in this dir so that the package comparison works
        // This is needed since the status_cache is added by the packager and is not collected from
        // the source dir for snapshots
        snapshot_utils::serialize_snapshot_data_file(
            &saved_snapshots_dir
                .path()
                .join(snapshot_utils::SNAPSHOT_STATUS_CACHE_FILE_NAME),
            |stream| {
                serialize_into(stream, &[] as &[BankSlotDelta])?;
                Ok(())
            },
        )
        .unwrap();

        snapshot_utils::verify_snapshot_archive(
            saved_archive_path.unwrap(),
            saved_snapshots_dir.path(),
            saved_accounts_dir
                .path()
                .join(accounts_dir.path().file_name().unwrap()),
            CompressionType::Bzip2,
        );
    }

    fn run_test_slots_to_snapshot(snapshot_version: SnapshotVersion, cluster_type: ClusterType) {
        solana_logger::setup();
        let num_set_roots = *MAX_CACHE_ENTRIES * 2;

        for add_root_interval in &[1, 3, 9] {
            let (snapshot_sender, _snapshot_receiver) = unbounded();
            // Make sure this test never clears bank.slots_since_snapshot
            let mut snapshot_test_config = SnapshotTestConfig::new(
                snapshot_version,
                cluster_type,
                (*add_root_interval * num_set_roots * 2) as u64,
            );
            let mut current_bank = snapshot_test_config.bank_forks[0].clone();
            let snapshot_sender = Some(snapshot_sender);
            for _ in 0..num_set_roots {
                for _ in 0..*add_root_interval {
                    let new_slot = current_bank.slot() + 1;
                    let new_bank =
                        Bank::new_from_parent(&current_bank, &Pubkey::default(), new_slot);
                    snapshot_test_config.bank_forks.insert(new_bank);
                    current_bank = snapshot_test_config.bank_forks[new_slot].clone();
                }
                snapshot_test_config.bank_forks.set_root(
                    current_bank.slot(),
                    &snapshot_sender,
                    None,
                );
            }

            let num_old_slots = num_set_roots * *add_root_interval - *MAX_CACHE_ENTRIES + 1;
            let expected_slots_to_snapshot =
                num_old_slots as u64..=num_set_roots as u64 * *add_root_interval as u64;

            let slots_to_snapshot = snapshot_test_config
                .bank_forks
                .get(snapshot_test_config.bank_forks.root())
                .unwrap()
                .src
                .roots();
            assert!(slots_to_snapshot.into_iter().eq(expected_slots_to_snapshot));
        }
    }

    fn run_test_bank_forks_status_cache_snapshot_n(
        snapshot_version: SnapshotVersion,
        cluster_type: ClusterType,
    ) {
        // create banks up to slot (MAX_CACHE_ENTRIES * 2) + 1 while transferring 1 lamport into 2 different accounts each time
        // this is done to ensure the AccountStorageEntries keep getting cleaned up as the root moves
        // ahead. Also tests the status_cache purge and status cache snapshotting.
        // Makes sure that the last bank is restored correctly
        let key1 = Keypair::new().pubkey();
        let key2 = Keypair::new().pubkey();
        for set_root_interval in &[1, 4] {
            run_bank_forks_snapshot_n(
                snapshot_version,
                cluster_type,
                (*MAX_CACHE_ENTRIES * 2 + 1) as u64,
                |bank, mint_keypair| {
                    let tx = system_transaction::transfer(
                        &mint_keypair,
                        &key1,
                        1,
                        bank.parent().unwrap().last_blockhash(),
                    );
                    assert_eq!(bank.process_transaction(&tx), Ok(()));
                    let tx = system_transaction::transfer(
                        &mint_keypair,
                        &key2,
                        1,
                        bank.parent().unwrap().last_blockhash(),
                    );
                    assert_eq!(bank.process_transaction(&tx), Ok(()));
                    goto_end_of_slot(bank);
                },
                *set_root_interval,
            );
        }
    }
}
