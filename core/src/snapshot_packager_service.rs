use crate::cluster_info::{ClusterInfo, CFG as CLUSTER_CFG};
use solana_runtime::{snapshot_package::AccountsPackageReceiver, snapshot_utils};
use solana_sdk::{clock::Slot, hash::Hash};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
        Arc,
    },
    thread::{self, Builder, JoinHandle},
    time::Duration,
};

pub struct SnapshotPackagerService {
    t_snapshot_packager: JoinHandle<()>,
}

impl SnapshotPackagerService {
    pub fn new(
        snapshot_package_receiver: AccountsPackageReceiver,
        starting_snapshot_hash: Option<(Slot, Hash)>,
        exit: &Arc<AtomicBool>,
        cluster_info: &Arc<ClusterInfo>,
    ) -> Self {
        let exit = exit.clone();
        let cluster_info = cluster_info.clone();

        let t_snapshot_packager = Builder::new()
            .name("solana-snapshot-packager".to_string())
            .spawn(move || {
                let mut hashes = vec![];
                if let Some(starting_snapshot_hash) = starting_snapshot_hash {
                    hashes.push(starting_snapshot_hash);
                }
                cluster_info.push_snapshot_hashes(hashes.clone());
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    match snapshot_package_receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(mut snapshot_package) => {
                            // Only package the latest
                            while let Ok(new_snapshot_package) =
                                snapshot_package_receiver.try_recv()
                            {
                                snapshot_package = new_snapshot_package;
                            }
                            if let Err(err) =
                                snapshot_utils::archive_snapshot_package(&snapshot_package)
                            {
                                warn!("Failed to create snapshot archive: {}", err);
                            } else {
                                hashes.push((snapshot_package.root, snapshot_package.hash));
                                while hashes.len() > CLUSTER_CFG.MAX_SNAPSHOT_HASHES {
                                    hashes.remove(0);
                                }
                                cluster_info.push_snapshot_hashes(hashes.clone());
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => (),
                    }
                }
            })
            .unwrap();
        Self {
            t_snapshot_packager,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_snapshot_packager.join()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::serialize_into;
    use solana_runtime::{
        accounts_db::AccountStorageEntry,
        bank::BankSlotDelta,
        bank_forks::CompressionType,
        snapshot_package::AccountsPackage,
        snapshot_utils::{self, SnapshotVersion, SNAPSHOT_STATUS_CACHE_FILE_NAME},
    };
    use solana_sdk::hash::Hash;
    use std::{
        fs::{self, remove_dir_all, OpenOptions},
        io::Write,
        path::{Path, PathBuf},
    };
    use tempfile::TempDir;

    // Create temporary placeholder directory for all test files
    fn make_tmp_dir_path() -> PathBuf {
        let out_dir = std::env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string());
        let path = PathBuf::from(format!("{}/tmp/test_package_snapshots", out_dir));

        // whack any possible collision
        let _ignored = std::fs::remove_dir_all(&path);
        // whack any possible collision
        let _ignored = std::fs::remove_file(&path);

        path
    }

    #[test]
    fn test_package_snapshots_relative_ledger_path() {
        let temp_dir = make_tmp_dir_path();
        create_and_verify_snapshot(&temp_dir);
        remove_dir_all(temp_dir).expect("should remove tmp dir");
    }

    #[test]
    fn test_package_snapshots() {
        create_and_verify_snapshot(TempDir::new().unwrap().path())
    }

    fn create_and_verify_snapshot(temp_dir: &Path) {
        let accounts_dir = temp_dir.join("accounts");
        let snapshots_dir = temp_dir.join("snapshots");
        let snapshot_package_output_path = temp_dir.join("snapshots_output");
        fs::create_dir_all(&snapshot_package_output_path).unwrap();

        fs::create_dir_all(&accounts_dir).unwrap();
        // Create some storage entries
        let storage_entries: Vec<_> = (0..5)
            .map(|i| Arc::new(AccountStorageEntry::new(&accounts_dir, 0, i, 10)))
            .collect();

        // Create some fake snapshot
        let snapshots_paths: Vec<_> = (0..5)
            .map(|i| {
                let snapshot_file_name = format!("{}", i);
                let snapshots_dir = snapshots_dir.join(&snapshot_file_name);
                fs::create_dir_all(&snapshots_dir).unwrap();
                let fake_snapshot_path = snapshots_dir.join(&snapshot_file_name);
                let mut fake_snapshot_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&fake_snapshot_path)
                    .unwrap();

                fake_snapshot_file.write_all(b"Hello, world!").unwrap();
                fake_snapshot_path
            })
            .collect();

        // Create directory of hard links for snapshots
        let link_snapshots_dir = tempfile::tempdir_in(&temp_dir).unwrap();
        for snapshots_path in snapshots_paths {
            let snapshot_file_name = snapshots_path.file_name().unwrap();
            let link_snapshots_dir = link_snapshots_dir.path().join(snapshot_file_name);
            fs::create_dir_all(&link_snapshots_dir).unwrap();
            let link_path = link_snapshots_dir.join(snapshot_file_name);
            fs::hard_link(&snapshots_path, &link_path).unwrap();
        }

        // Create a packageable snapshot
        let output_tar_path = snapshot_utils::get_snapshot_archive_path(
            &snapshot_package_output_path,
            &(42, Hash::default()),
            &CompressionType::Bzip2,
        );
        let snapshot_package = AccountsPackage::new(
            5,
            5,
            vec![],
            link_snapshots_dir,
            vec![storage_entries],
            output_tar_path.clone(),
            Hash::default(),
            CompressionType::Bzip2,
            SnapshotVersion::default(),
        );

        // Make tarball from packageable snapshot
        snapshot_utils::archive_snapshot_package(&snapshot_package).unwrap();

        // before we compare, stick an empty status_cache in this dir so that the package comparison works
        // This is needed since the status_cache is added by the packager and is not collected from
        // the source dir for snapshots
        let dummy_slot_deltas: Vec<BankSlotDelta> = vec![];
        snapshot_utils::serialize_snapshot_data_file(
            &snapshots_dir.join(SNAPSHOT_STATUS_CACHE_FILE_NAME),
            |stream| {
                serialize_into(stream, &dummy_slot_deltas)?;
                Ok(())
            },
        )
        .unwrap();

        // Check archive is correct
        snapshot_utils::verify_snapshot_archive(
            output_tar_path,
            snapshots_dir,
            accounts_dir,
            CompressionType::Bzip2,
        );
    }
}
