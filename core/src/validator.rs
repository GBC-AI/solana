//! The `validator` module hosts all the validator microservices.

use crate::{
    broadcast_stage::BroadcastStageType,
    cache_block_time_service::{CacheBlockTimeSender, CacheBlockTimeService},
    cluster_info::{ClusterInfo, Node},
    cluster_info_vote_listener::VoteTracker,
    completed_data_sets_service::CompletedDataSetsService,
    consensus::{reconcile_blockstore_roots_with_tower, Tower},
    contact_info::ContactInfo,
    gossip_service::GossipService,
    optimistically_confirmed_bank_tracker::{
        OptimisticallyConfirmedBank, OptimisticallyConfirmedBankTracker,
    },
    poh_recorder::{PohRecorder, CFG as POH_CFG},
    poh_service::PohService,
    rewards_recorder_service::{RewardsRecorderSender, RewardsRecorderService},
    rpc::JsonRpcConfig,
    rpc_pubsub_service::{PubSubConfig, PubSubService},
    rpc_service::JsonRpcService,
    rpc_subscriptions::RpcSubscriptions,
    sample_performance_service::SamplePerformanceService,
    serve_repair::ServeRepair,
    serve_repair_service::ServeRepairService,
    sigverify,
    snapshot_packager_service::SnapshotPackagerService,
    tpu::Tpu,
    transaction_status_service::TransactionStatusService,
    tvu::{Sockets, Tvu, TvuConfig},
};
use crossbeam_channel::{bounded, unbounded};
use rand::{thread_rng, Rng};
use solana_banks_server::rpc_banks_service::RpcBanksService;
use solana_ledger::{
    bank_forks_utils,
    blockstore::{Blockstore, BlockstoreSignals, CompletedSlotsReceiver, PurgeType},
    blockstore_db::BlockstoreRecoveryMode,
    blockstore_processor::{self, TransactionStatusSender},
    leader_schedule::FixedSchedule,
    leader_schedule_cache::LeaderScheduleCache,
};
use solana_measure::measure::Measure;
use solana_metrics::datapoint_info;
use solana_runtime::{
    bank::Bank,
    bank_forks::{BankForks, SnapshotConfig},
    commitment::BlockCommitmentCache,
    hardened_unpack::{open_genesis_config, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE},
};
use solana_sdk::{
    clock::Slot,
    epoch_schedule::CFG as EPOCH_CFG,
    genesis_config::GenesisConfig,
    hash::Hash,
    pubkey::Pubkey,
    shred_version::compute_shred_version,
    signature::{Keypair, Signer},
    timing::timestamp,
};
use solana_vote_program::vote_state::VoteState;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicBool, Ordering},
    sync::mpsc::Receiver,
    sync::{mpsc::channel, Arc, Mutex, RwLock},
    thread::{sleep, Result},
    time::Duration,
};

const MAX_COMPLETED_DATA_SETS_IN_CHANNEL: usize = 100_000;

#[derive(Clone, Debug)]
pub struct ValidatorConfig {
    pub dev_halt_at_slot: Option<Slot>,
    pub expected_genesis_hash: Option<Hash>,
    pub expected_bank_hash: Option<Hash>,
    pub expected_shred_version: Option<u16>,
    pub voting_disabled: bool,
    pub account_paths: Vec<PathBuf>,
    pub rpc_config: JsonRpcConfig,
    pub rpc_addrs: Option<(SocketAddr, SocketAddr, SocketAddr)>, // (JsonRpc, JsonRpcPubSub, Banks)
    pub pubsub_config: PubSubConfig,
    pub snapshot_config: Option<SnapshotConfig>,
    pub max_ledger_shreds: Option<u64>,
    pub broadcast_stage_type: BroadcastStageType,
    pub enable_partition: Option<Arc<AtomicBool>>,
    pub fixed_leader_schedule: Option<FixedSchedule>,
    pub wait_for_supermajority: Option<Slot>,
    pub new_hard_forks: Option<Vec<Slot>>,
    pub trusted_validators: Option<HashSet<Pubkey>>, // None = trust all
    pub repair_validators: Option<HashSet<Pubkey>>,  // None = repair from all
    pub gossip_validators: Option<HashSet<Pubkey>>,  // None = gossip with all
    pub halt_on_trusted_validators_accounts_hash_mismatch: bool,
    pub accounts_hash_fault_injection_slots: u64, // 0 = no fault injection
    pub frozen_accounts: Vec<Pubkey>,
    pub no_rocksdb_compaction: bool,
    pub accounts_hash_interval_slots: u64,
    pub max_genesis_archive_unpacked_size: u64,
    pub wal_recovery_mode: Option<BlockstoreRecoveryMode>,
    pub poh_verify: bool, // Perform PoH verification during blockstore processing at boo
    pub cuda: bool,
    pub require_tower: bool,
    pub debug_keys: Option<Arc<HashSet<Pubkey>>>,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            dev_halt_at_slot: None,
            expected_genesis_hash: None,
            expected_bank_hash: None,
            expected_shred_version: None,
            voting_disabled: false,
            max_ledger_shreds: None,
            account_paths: Vec::new(),
            rpc_config: JsonRpcConfig::default(),
            rpc_addrs: None,
            pubsub_config: PubSubConfig::default(),
            snapshot_config: None,
            broadcast_stage_type: BroadcastStageType::Standard,
            enable_partition: None,
            fixed_leader_schedule: None,
            wait_for_supermajority: None,
            new_hard_forks: None,
            trusted_validators: None,
            repair_validators: None,
            gossip_validators: None,
            halt_on_trusted_validators_accounts_hash_mismatch: false,
            accounts_hash_fault_injection_slots: 0,
            frozen_accounts: vec![],
            no_rocksdb_compaction: false,
            accounts_hash_interval_slots: std::u64::MAX,
            max_genesis_archive_unpacked_size: MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            wal_recovery_mode: None,
            poh_verify: true,
            cuda: false,
            require_tower: false,
            debug_keys: None,
        }
    }
}

#[derive(Default)]
pub struct ValidatorExit {
    exits: Vec<Box<dyn FnOnce() + Send + Sync>>,
}

impl ValidatorExit {
    pub fn register_exit(&mut self, exit: Box<dyn FnOnce() + Send + Sync>) {
        self.exits.push(exit);
    }

    pub fn exit(self) {
        for exit in self.exits {
            exit();
        }
    }
}

#[derive(Default)]
struct TransactionHistoryServices {
    transaction_status_sender: Option<TransactionStatusSender>,
    transaction_status_service: Option<TransactionStatusService>,
    rewards_recorder_sender: Option<RewardsRecorderSender>,
    rewards_recorder_service: Option<RewardsRecorderService>,
    cache_block_time_sender: Option<CacheBlockTimeSender>,
    cache_block_time_service: Option<CacheBlockTimeService>,
}

struct RpcServices {
    json_rpc_service: JsonRpcService,
    pubsub_service: PubSubService,
    rpc_banks_service: RpcBanksService,
    optimistically_confirmed_bank_tracker: OptimisticallyConfirmedBankTracker,
}

pub struct Validator {
    pub id: Pubkey,
    validator_exit: Arc<RwLock<Option<ValidatorExit>>>,
    rpc_service: Option<RpcServices>,
    transaction_status_service: Option<TransactionStatusService>,
    rewards_recorder_service: Option<RewardsRecorderService>,
    cache_block_time_service: Option<CacheBlockTimeService>,
    sample_performance_service: Option<SamplePerformanceService>,
    gossip_service: GossipService,
    serve_repair_service: ServeRepairService,
    completed_data_sets_service: CompletedDataSetsService,
    snapshot_packager_service: Option<SnapshotPackagerService>,
    poh_recorder: Arc<Mutex<PohRecorder>>,
    poh_service: PohService,
    tpu: Tpu,
    tvu: Tvu,
    ip_echo_server: solana_net_utils::IpEchoServer,
}

impl Validator {
    pub fn new(
        mut node: Node,
        identity_keypair: &Arc<Keypair>,
        ledger_path: &Path,
        vote_account: &Pubkey,
        mut authorized_voter_keypairs: Vec<Arc<Keypair>>,
        cluster_entrypoint: Option<&ContactInfo>,
        config: &ValidatorConfig,
    ) -> Self {
        let id = identity_keypair.pubkey();
        assert_eq!(id, node.info.id);

        warn!("identity: {}", id);
        warn!("vote account: {}", vote_account);

        if config.voting_disabled {
            warn!("voting disabled");
            authorized_voter_keypairs.clear();
        } else {
            for authorized_voter_keypair in &authorized_voter_keypairs {
                warn!("authorized voter: {}", authorized_voter_keypair.pubkey());
            }
        }
        report_target_features();

        info!("entrypoint: {:?}", cluster_entrypoint);

        if solana_perf::perf_libs::api().is_some() {
            info!("Initializing sigverify, this could take a while...");
        } else {
            info!("Initializing sigverify...");
        }
        sigverify::init();
        info!("Done.");

        if !ledger_path.is_dir() {
            error!(
                "ledger directory does not exist or is not accessible: {:?}",
                ledger_path
            );
            process::exit(1);
        }

        if let Some(shred_version) = config.expected_shred_version {
            if let Some(wait_for_supermajority_slot) = config.wait_for_supermajority {
                backup_and_clear_blockstore(
                    ledger_path,
                    wait_for_supermajority_slot + 1,
                    shred_version,
                );
            }
        }

        info!("Cleaning accounts paths..");
        let mut start = Measure::start("clean_accounts_paths");
        for accounts_path in &config.account_paths {
            cleanup_accounts_path(accounts_path);
        }
        start.stop();
        info!("done. {}", start);

        let mut validator_exit = ValidatorExit::default();
        let exit = Arc::new(AtomicBool::new(false));
        let exit_ = exit.clone();
        validator_exit.register_exit(Box::new(move || exit_.store(true, Ordering::Relaxed)));
        let validator_exit = Arc::new(RwLock::new(Some(validator_exit)));

        let (replay_vote_sender, replay_vote_receiver) = unbounded();
        let (
            genesis_config,
            bank_forks,
            blockstore,
            ledger_signal_receiver,
            completed_slots_receiver,
            leader_schedule_cache,
            snapshot_hash,
            TransactionHistoryServices {
                transaction_status_sender,
                transaction_status_service,
                rewards_recorder_sender,
                rewards_recorder_service,
                cache_block_time_sender,
                cache_block_time_service,
            },
            tower,
        ) = new_banks_from_ledger(
            &id,
            vote_account,
            config,
            ledger_path,
            config.poh_verify,
            &exit,
        );

        let leader_schedule_cache = Arc::new(leader_schedule_cache);
        let bank = bank_forks.working_bank();
        let bank_forks = Arc::new(RwLock::new(bank_forks));

        let sample_performance_service =
            if config.rpc_addrs.is_some() && config.rpc_config.enable_rpc_transaction_history {
                Some(SamplePerformanceService::new(
                    &bank_forks,
                    &blockstore,
                    &exit,
                ))
            } else {
                None
            };

        info!("Starting validator with working bank slot {}", bank.slot());
        {
            let hard_forks: Vec<_> = bank.hard_forks().read().unwrap().iter().copied().collect();
            if !hard_forks.is_empty() {
                info!("Hard forks: {:?}", hard_forks);
            }
        }

        node.info.wallclock = timestamp();
        node.info.shred_version = compute_shred_version(
            &genesis_config.hash(),
            Some(&bank.hard_forks().read().unwrap()),
        );

        Self::print_node_info(&node);

        if let Some(expected_shred_version) = config.expected_shred_version {
            if expected_shred_version != node.info.shred_version {
                error!(
                    "shred version mismatch: expected {} found: {}",
                    expected_shred_version, node.info.shred_version,
                );
                process::exit(1);
            }
        }

        let cluster_info = Arc::new(ClusterInfo::new(
            node.info.clone(),
            identity_keypair.clone(),
        ));
        let mut block_commitment_cache = BlockCommitmentCache::default();
        block_commitment_cache.initialize_slots(bank.slot());
        let block_commitment_cache = Arc::new(RwLock::new(block_commitment_cache));

        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);

        let subscriptions = Arc::new(RpcSubscriptions::new(
            &exit,
            bank_forks.clone(),
            block_commitment_cache.clone(),
            optimistically_confirmed_bank.clone(),
        ));

        let (completed_data_sets_sender, completed_data_sets_receiver) =
            bounded(MAX_COMPLETED_DATA_SETS_IN_CHANNEL);
        let completed_data_sets_service = CompletedDataSetsService::new(
            completed_data_sets_receiver,
            blockstore.clone(),
            subscriptions.clone(),
            &exit,
        );

        info!(
            "Starting PoH: epoch={} slot={} tick_height={} blockhash={} leader={:?}",
            bank.epoch(),
            bank.slot(),
            bank.tick_height(),
            bank.last_blockhash(),
            leader_schedule_cache.slot_leader_at(bank.slot(), Some(&bank))
        );

        let poh_config = Arc::new(genesis_config.poh_config.clone());
        let (mut poh_recorder, entry_receiver) = PohRecorder::new_with_clear_signal(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.slot(),
            leader_schedule_cache.next_leader_slot(
                &id,
                bank.slot(),
                &bank,
                Some(&blockstore),
                POH_CFG.GRACE_TICKS_FACTOR * POH_CFG.MAX_GRACE_SLOTS,
            ),
            bank.ticks_per_slot(),
            &id,
            &blockstore,
            blockstore.new_shreds_signals.first().cloned(),
            &leader_schedule_cache,
            &poh_config,
        );
        if config.snapshot_config.is_some() {
            poh_recorder.set_bank(&bank);
        }
        let poh_recorder = Arc::new(Mutex::new(poh_recorder));

        let rpc_override_health_check = Arc::new(AtomicBool::new(false));
        let (rpc_service, bank_notification_sender) =
            if let Some((rpc_addr, rpc_pubsub_addr, rpc_banks_addr)) = config.rpc_addrs {
                if ContactInfo::is_valid_address(&node.info.rpc) {
                    assert!(ContactInfo::is_valid_address(&node.info.rpc_pubsub));
                    assert!(ContactInfo::is_valid_address(&node.info.rpc_banks));
                } else {
                    assert!(!ContactInfo::is_valid_address(&node.info.rpc_pubsub));
                    assert!(!ContactInfo::is_valid_address(&node.info.rpc_banks));
                }
                let tpu_address = cluster_info.my_contact_info().tpu;
                let (bank_notification_sender, bank_notification_receiver) = unbounded();
                (
                    Some(RpcServices {
                        json_rpc_service: JsonRpcService::new(
                            rpc_addr,
                            config.rpc_config.clone(),
                            config.snapshot_config.clone(),
                            bank_forks.clone(),
                            block_commitment_cache.clone(),
                            blockstore.clone(),
                            cluster_info.clone(),
                            Some(poh_recorder.clone()),
                            genesis_config.hash(),
                            ledger_path,
                            validator_exit.clone(),
                            config.trusted_validators.clone(),
                            rpc_override_health_check.clone(),
                            optimistically_confirmed_bank.clone(),
                        ),
                        pubsub_service: PubSubService::new(
                            config.pubsub_config.clone(),
                            &subscriptions,
                            rpc_pubsub_addr,
                            &exit,
                        ),
                        rpc_banks_service: RpcBanksService::new(
                            rpc_banks_addr,
                            tpu_address,
                            &bank_forks,
                            &block_commitment_cache,
                            &exit,
                        ),
                        optimistically_confirmed_bank_tracker:
                            OptimisticallyConfirmedBankTracker::new(
                                bank_notification_receiver,
                                &exit,
                                bank_forks.clone(),
                                optimistically_confirmed_bank,
                                subscriptions.clone(),
                            ),
                    }),
                    Some(bank_notification_sender),
                )
            } else {
                (None, None)
            };

        if config.dev_halt_at_slot.is_some() {
            // Simulate a confirmed root to avoid RPC errors with CommitmentConfig::max() and
            // to ensure RPC endpoints like getConfirmedBlock, which require a confirmed root, work
            block_commitment_cache
                .write()
                .unwrap()
                .set_highest_confirmed_root(bank_forks.read().unwrap().root());

            // Park with the RPC service running, ready for inspection!
            warn!("Validator halted");
            std::thread::park();
        }

        let ip_echo_server = solana_net_utils::ip_echo_server(node.sockets.ip_echo.unwrap());

        let gossip_service = GossipService::new(
            &cluster_info,
            Some(bank_forks.clone()),
            node.sockets.gossip,
            config.gossip_validators.clone(),
            &exit,
        );

        let serve_repair = Arc::new(RwLock::new(ServeRepair::new(cluster_info.clone())));
        let serve_repair_service = ServeRepairService::new(
            &serve_repair,
            Some(blockstore.clone()),
            node.sockets.serve_repair,
            &exit,
        );

        // Insert the entrypoint info, should only be None if this node
        // is the bootstrap validator
        if let Some(cluster_entrypoint) = cluster_entrypoint {
            cluster_info.set_entrypoint(cluster_entrypoint.clone());
        }

        let (snapshot_packager_service, snapshot_config_and_package_sender) =
            if let Some(snapshot_config) = config.snapshot_config.clone() {
                // Start a snapshot packaging service
                let (sender, receiver) = channel();
                let snapshot_packager_service =
                    SnapshotPackagerService::new(receiver, snapshot_hash, &exit, &cluster_info);
                (
                    Some(snapshot_packager_service),
                    Some((snapshot_config, sender)),
                )
            } else {
                (None, None)
            };

        if wait_for_supermajority(config, &bank, &cluster_info, rpc_override_health_check) {
            std::process::exit(1);
        }

        let poh_service = PohService::new(poh_recorder.clone(), &poh_config, &exit);
        assert_eq!(
            blockstore.new_shreds_signals.len(),
            1,
            "New shred signal for the TVU should be the same as the clear bank signal."
        );

        let vote_tracker = Arc::new(VoteTracker::new(bank_forks.read().unwrap().root_bank()));

        let (retransmit_slots_sender, retransmit_slots_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();
        let tvu = Tvu::new(
            vote_account,
            authorized_voter_keypairs,
            &bank_forks,
            &cluster_info,
            Sockets {
                repair: node
                    .sockets
                    .repair
                    .try_clone()
                    .expect("Failed to clone repair socket"),
                retransmit: node
                    .sockets
                    .retransmit_sockets
                    .iter()
                    .map(|s| s.try_clone().expect("Failed to clone retransmit socket"))
                    .collect(),
                fetch: node
                    .sockets
                    .tvu
                    .iter()
                    .map(|s| s.try_clone().expect("Failed to clone TVU Sockets"))
                    .collect(),
                forwards: node
                    .sockets
                    .tvu_forwards
                    .iter()
                    .map(|s| s.try_clone().expect("Failed to clone TVU forwards Sockets"))
                    .collect(),
            },
            blockstore.clone(),
            ledger_signal_receiver,
            &subscriptions,
            &poh_recorder,
            tower,
            &leader_schedule_cache,
            &exit,
            completed_slots_receiver,
            block_commitment_cache,
            config.enable_partition.clone(),
            transaction_status_sender.clone(),
            rewards_recorder_sender,
            cache_block_time_sender,
            snapshot_config_and_package_sender,
            vote_tracker.clone(),
            retransmit_slots_sender,
            verified_vote_receiver,
            replay_vote_sender.clone(),
            completed_data_sets_sender,
            bank_notification_sender.clone(),
            TvuConfig {
                max_ledger_shreds: config.max_ledger_shreds,
                halt_on_trusted_validators_accounts_hash_mismatch: config
                    .halt_on_trusted_validators_accounts_hash_mismatch,
                shred_version: node.info.shred_version,
                trusted_validators: config.trusted_validators.clone(),
                repair_validators: config.repair_validators.clone(),
                accounts_hash_fault_injection_slots: config.accounts_hash_fault_injection_slots,
            },
        );

        let tpu = Tpu::new(
            &cluster_info,
            &poh_recorder,
            entry_receiver,
            retransmit_slots_receiver,
            node.sockets.tpu,
            node.sockets.tpu_forwards,
            node.sockets.broadcast,
            &subscriptions,
            transaction_status_sender,
            &blockstore,
            &config.broadcast_stage_type,
            &exit,
            node.info.shred_version,
            vote_tracker,
            bank_forks,
            verified_vote_sender,
            replay_vote_receiver,
            replay_vote_sender,
            bank_notification_sender,
        );

        datapoint_info!("validator-new", ("id", id.to_string(), String));
        Self {
            id,
            gossip_service,
            serve_repair_service,
            rpc_service,
            transaction_status_service,
            rewards_recorder_service,
            cache_block_time_service,
            sample_performance_service,
            snapshot_packager_service,
            completed_data_sets_service,
            tpu,
            tvu,
            poh_service,
            poh_recorder,
            ip_echo_server,
            validator_exit,
        }
    }

    // Used for notifying many nodes in parallel to exit
    pub fn exit(&mut self) {
        if let Some(x) = self.validator_exit.write().unwrap().take() {
            x.exit()
        }
    }

    pub fn close(mut self) -> Result<()> {
        self.exit();
        self.join()
    }

    fn print_node_info(node: &Node) {
        info!("{:?}", node.info);
        info!(
            "local gossip address: {}",
            node.sockets.gossip.local_addr().unwrap()
        );
        info!(
            "local broadcast address: {}",
            node.sockets
                .broadcast
                .first()
                .unwrap()
                .local_addr()
                .unwrap()
        );
        info!(
            "local repair address: {}",
            node.sockets.repair.local_addr().unwrap()
        );
        info!(
            "local retransmit address: {}",
            node.sockets.retransmit_sockets[0].local_addr().unwrap()
        );
    }

    pub fn join(self) -> Result<()> {
        self.poh_service.join()?;
        drop(self.poh_recorder);
        if let Some(RpcServices {
            json_rpc_service,
            pubsub_service,
            rpc_banks_service,
            optimistically_confirmed_bank_tracker,
        }) = self.rpc_service
        {
            json_rpc_service.join()?;
            pubsub_service.join()?;
            rpc_banks_service.join()?;
            optimistically_confirmed_bank_tracker.join()?;
        }
        if let Some(transaction_status_service) = self.transaction_status_service {
            transaction_status_service.join()?;
        }

        if let Some(rewards_recorder_service) = self.rewards_recorder_service {
            rewards_recorder_service.join()?;
        }

        if let Some(cache_block_time_service) = self.cache_block_time_service {
            cache_block_time_service.join()?;
        }

        if let Some(sample_performance_service) = self.sample_performance_service {
            sample_performance_service.join()?;
        }

        if let Some(s) = self.snapshot_packager_service {
            s.join()?;
        }

        self.gossip_service.join()?;
        self.serve_repair_service.join()?;
        self.tpu.join()?;
        self.tvu.join()?;
        self.completed_data_sets_service.join()?;
        self.ip_echo_server.shutdown_now();

        Ok(())
    }
}

fn active_vote_account_exists_in_bank(bank: &Arc<Bank>, vote_account: &Pubkey) -> bool {
    if let Some(account) = &bank.get_account(vote_account) {
        if let Some(vote_state) = VoteState::from(&account) {
            return !vote_state.votes.is_empty();
        }
    }
    false
}

fn post_process_restored_tower(
    restored_tower: crate::consensus::Result<Tower>,
    validator_identity: &Pubkey,
    vote_account: &Pubkey,
    config: &ValidatorConfig,
    ledger_path: &Path,
    bank_forks: &BankForks,
) -> Tower {
    restored_tower
        .and_then(|tower| {
            let root_bank = bank_forks.root_bank();
            let slot_history = root_bank.get_slot_history();
            tower.adjust_lockouts_after_replay(root_bank.slot(), &slot_history)
        })
        .unwrap_or_else(|err| {
            let voting_has_been_active =
                active_vote_account_exists_in_bank(&bank_forks.working_bank(), &vote_account);
            if !err.is_file_missing() {
                datapoint_error!(
                    "tower_error",
                    (
                        "error",
                        format!("Unable to restore tower: {}", err),
                        String
                    ),
                );
            }
            if config.require_tower && voting_has_been_active {
                error!("Requested mandatory tower restore failed: {}", err);
                error!(
                    "And there is an existing vote_account containing actual votes. \
                     Aborting due to possible conflicting duplicate votes",
                );
                process::exit(1);
            }
            if err.is_file_missing() && !voting_has_been_active {
                // Currently, don't protect against spoofed snapshots with no tower at all
                info!(
                    "Ignoring expected failed tower restore because this is the initial \
                      validator start with the vote account..."
                );
            } else {
                error!(
                    "Rebuilding a new tower from the latest vote account due to failed tower restore: {}",
                    err
                );
            }

            Tower::new_from_bankforks(
                &bank_forks,
                &ledger_path,
                &validator_identity,
                &vote_account,
            )
        })
}

#[allow(clippy::type_complexity)]
fn new_banks_from_ledger(
    validator_identity: &Pubkey,
    vote_account: &Pubkey,
    config: &ValidatorConfig,
    ledger_path: &Path,
    poh_verify: bool,
    exit: &Arc<AtomicBool>,
) -> (
    GenesisConfig,
    BankForks,
    Arc<Blockstore>,
    Receiver<bool>,
    CompletedSlotsReceiver,
    LeaderScheduleCache,
    Option<(Slot, Hash)>,
    TransactionHistoryServices,
    Tower,
) {
    info!("loading ledger from {:?}...", ledger_path);
    let genesis_config = open_genesis_config(ledger_path, config.max_genesis_archive_unpacked_size);

    // This needs to be limited otherwise the state in the VoteAccount data
    // grows too large
    let leader_schedule_slot_offset = genesis_config.epoch_schedule.leader_schedule_slot_offset;
    let slots_per_epoch = genesis_config.epoch_schedule.slots_per_epoch;
    let leader_epoch_offset = (leader_schedule_slot_offset + slots_per_epoch - 1) / slots_per_epoch;
    assert!(leader_epoch_offset <= EPOCH_CFG.MAX_LEADER_SCHEDULE_EPOCH_OFFSET);

    let genesis_hash = genesis_config.hash();
    info!("genesis hash: {}", genesis_hash);

    if let Some(expected_genesis_hash) = config.expected_genesis_hash {
        if genesis_hash != expected_genesis_hash {
            error!("genesis hash mismatch: expected {}", expected_genesis_hash);
            error!("Delete the ledger directory to continue: {:?}", ledger_path);
            process::exit(1);
        }
    }

    let BlockstoreSignals {
        mut blockstore,
        ledger_signal_receiver,
        completed_slots_receiver,
        ..
    } = Blockstore::open_with_signal(ledger_path, config.wal_recovery_mode.clone())
        .expect("Failed to open ledger database");
    blockstore.set_no_compaction(config.no_rocksdb_compaction);

    let restored_tower = Tower::restore(ledger_path, &validator_identity);
    if let Ok(tower) = &restored_tower {
        reconcile_blockstore_roots_with_tower(&tower, &blockstore).unwrap_or_else(|err| {
            error!("Failed to reconcile blockstore with tower: {:?}", err);
            std::process::exit(1);
        });
    }

    let process_options = blockstore_processor::ProcessOptions {
        poh_verify,
        dev_halt_at_slot: config.dev_halt_at_slot,
        new_hard_forks: config.new_hard_forks.clone(),
        frozen_accounts: config.frozen_accounts.clone(),
        debug_keys: config.debug_keys.clone(),
        ..blockstore_processor::ProcessOptions::default()
    };

    let blockstore = Arc::new(blockstore);
    let transaction_history_services =
        if config.rpc_addrs.is_some() && config.rpc_config.enable_rpc_transaction_history {
            initialize_rpc_transaction_history_services(blockstore.clone(), exit)
        } else {
            TransactionHistoryServices::default()
        };

    let (mut bank_forks, mut leader_schedule_cache, snapshot_hash) = bank_forks_utils::load(
        &genesis_config,
        &blockstore,
        config.account_paths.clone(),
        config.snapshot_config.as_ref(),
        process_options,
        transaction_history_services
            .transaction_status_sender
            .clone(),
    )
    .unwrap_or_else(|err| {
        error!("Failed to load ledger: {:?}", err);
        process::exit(1);
    });

    let tower = post_process_restored_tower(
        restored_tower,
        &validator_identity,
        &vote_account,
        &config,
        &ledger_path,
        &bank_forks,
    );

    info!("Tower state: {:?}", tower);

    leader_schedule_cache.set_fixed_leader_schedule(config.fixed_leader_schedule.clone());

    bank_forks.set_snapshot_config(config.snapshot_config.clone());
    bank_forks.set_accounts_hash_interval_slots(config.accounts_hash_interval_slots);

    (
        genesis_config,
        bank_forks,
        blockstore,
        ledger_signal_receiver,
        completed_slots_receiver,
        leader_schedule_cache,
        snapshot_hash,
        transaction_history_services,
        tower,
    )
}

fn backup_and_clear_blockstore(ledger_path: &Path, start_slot: Slot, shred_version: u16) {
    use std::time::Instant;
    let blockstore = Blockstore::open(ledger_path).unwrap();
    let mut do_copy_and_clear = false;

    // Search for shreds with incompatible version in blockstore
    if let Ok(slot_meta_iterator) = blockstore.slot_meta_iterator(start_slot) {
        for (slot, _meta) in slot_meta_iterator {
            if let Ok(shreds) = blockstore.get_data_shreds_for_slot(slot, 0) {
                for shred in &shreds {
                    if shred.version() != shred_version {
                        do_copy_and_clear = true;
                        break;
                    }
                }
            }
        }
    }

    // If found, then copy shreds to another db and clear from start_slot
    if do_copy_and_clear {
        let folder_name = format!("backup_rocksdb_{}", thread_rng().gen_range(0, 99999));
        let backup_blockstore = Blockstore::open(&ledger_path.join(folder_name));
        let mut last_print = Instant::now();
        let mut copied = 0;
        let mut last_slot = None;
        let slot_meta_iterator = blockstore.slot_meta_iterator(start_slot).unwrap();
        for (slot, _meta) in slot_meta_iterator {
            if let Ok(shreds) = blockstore.get_data_shreds_for_slot(slot, 0) {
                if let Ok(ref backup_blockstore) = backup_blockstore {
                    copied += shreds.len();
                    let _ = backup_blockstore.insert_shreds(shreds, None, true);
                }
            }
            if last_print.elapsed().as_millis() > 3000 {
                info!(
                    "Copying shreds from slot {} copied {} so far.",
                    start_slot, copied
                );
                last_print = Instant::now();
            }
            last_slot = Some(slot);
        }

        let end_slot = last_slot.unwrap();
        info!("Purging slots {} to {}", start_slot, end_slot);
        blockstore.purge_from_next_slots(start_slot, end_slot);
        blockstore.purge_slots(start_slot, end_slot, PurgeType::Exact);
        info!("Purging done, compacting db..");
        if let Err(e) = blockstore.compact_storage(start_slot, end_slot) {
            warn!(
                "Error from compacting storage from {} to {}: {:?}",
                start_slot, end_slot, e
            );
        }
        info!("done");
    }
    drop(blockstore);
}

fn initialize_rpc_transaction_history_services(
    blockstore: Arc<Blockstore>,
    exit: &Arc<AtomicBool>,
) -> TransactionHistoryServices {
    let (transaction_status_sender, transaction_status_receiver) = unbounded();
    let transaction_status_sender = Some(transaction_status_sender);
    let transaction_status_service = Some(TransactionStatusService::new(
        transaction_status_receiver,
        blockstore.clone(),
        exit,
    ));

    let (rewards_recorder_sender, rewards_receiver) = unbounded();
    let rewards_recorder_sender = Some(rewards_recorder_sender);
    let rewards_recorder_service = Some(RewardsRecorderService::new(
        rewards_receiver,
        blockstore.clone(),
        exit,
    ));

    let (cache_block_time_sender, cache_block_time_receiver) = unbounded();
    let cache_block_time_sender = Some(cache_block_time_sender);
    let cache_block_time_service = Some(CacheBlockTimeService::new(
        cache_block_time_receiver,
        blockstore,
        exit,
    ));
    TransactionHistoryServices {
        transaction_status_sender,
        transaction_status_service,
        rewards_recorder_sender,
        rewards_recorder_service,
        cache_block_time_sender,
        cache_block_time_service,
    }
}

// Return true on error, indicating the validator should exit.
fn wait_for_supermajority(
    config: &ValidatorConfig,
    bank: &Bank,
    cluster_info: &ClusterInfo,
    rpc_override_health_check: Arc<AtomicBool>,
) -> bool {
    if let Some(wait_for_supermajority) = config.wait_for_supermajority {
        match wait_for_supermajority.cmp(&bank.slot()) {
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Greater => {
                error!("Ledger does not have enough data to wait for supermajority, please enable snapshot fetch. Has {} needs {}", bank.slot(), wait_for_supermajority);
                return true;
            }
            _ => {}
        }
    } else {
        return false;
    }

    if let Some(expected_bank_hash) = config.expected_bank_hash {
        if bank.hash() != expected_bank_hash {
            error!(
                "Bank hash({}) does not match expected value: {}",
                bank.hash(),
                expected_bank_hash
            );
            return true;
        }
    }

    info!(
        "Waiting for 80% of activated stake at slot {} to be in gossip...",
        bank.slot()
    );
    for i in 1.. {
        let gossip_stake_percent = get_stake_percent_in_gossip(&bank, &cluster_info, i % 10 == 0);

        if gossip_stake_percent >= 80 {
            break;
        }
        // The normal RPC health checks don't apply as the node is waiting, so feign health to
        // prevent load balancers from removing the node from their list of candidates during a
        // manual restart.
        rpc_override_health_check.store(true, Ordering::Relaxed);
        sleep(Duration::new(1, 0));
    }
    rpc_override_health_check.store(false, Ordering::Relaxed);
    false
}

fn report_target_features() {
    warn!(
        "CUDA is {}abled",
        if solana_perf::perf_libs::api().is_some() {
            "en"
        } else {
            "dis"
        }
    );

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        // Validator binaries built on a machine with AVX support will generate invalid opcodes
        // when run on machines without AVX causing a non-obvious process abort.  Instead detect
        // the mismatch and error cleanly.
        #[target_feature(enable = "avx")]
        {
            if is_x86_feature_detected!("avx") {
                info!("AVX detected");
            } else {
                error!("Your machine does not have AVX support, please rebuild from source on your machine");
                process::exit(1);
            }
        }
    }
}

// Get the activated stake percentage (based on the provided bank) that is visible in gossip
fn get_stake_percent_in_gossip(bank: &Bank, cluster_info: &ClusterInfo, log: bool) -> u64 {
    let mut online_stake = 0;
    let mut wrong_shred_stake = 0;
    let mut wrong_shred_nodes = vec![];
    let mut offline_stake = 0;
    let mut offline_nodes = vec![];

    let mut total_activated_stake = 0;
    let all_tvu_peers = cluster_info.all_tvu_peers();
    let my_shred_version = cluster_info.my_shred_version();
    let my_id = cluster_info.id();

    for (activated_stake, vote_account) in bank.vote_accounts().values() {
        let vote_state = VoteState::from(&vote_account).unwrap_or_default();
        total_activated_stake += activated_stake;

        if *activated_stake == 0 {
            continue;
        }

        if let Some(peer) = all_tvu_peers
            .iter()
            .find(|peer| peer.id == vote_state.node_pubkey)
        {
            if peer.shred_version == my_shred_version {
                trace!(
                    "observed {} in gossip, (activated_stake={})",
                    vote_state.node_pubkey,
                    activated_stake
                );
                online_stake += activated_stake;
            } else {
                wrong_shred_stake += activated_stake;
                wrong_shred_nodes.push((*activated_stake, vote_state.node_pubkey));
            }
        } else if vote_state.node_pubkey == my_id {
            online_stake += activated_stake; // This node is online
        } else {
            offline_stake += activated_stake;
            offline_nodes.push((*activated_stake, vote_state.node_pubkey));
        }
    }

    if log {
        info!(
            "{}% of active stake visible in gossip",
            online_stake * 100 / total_activated_stake
        );

        if !wrong_shred_nodes.is_empty() {
            info!(
                "{}% of active stake has the wrong shred version in gossip",
                wrong_shred_stake * 100 / total_activated_stake,
            );
            for (stake, identity) in wrong_shred_nodes {
                info!(
                    "    {}% - {}",
                    stake * 100 / total_activated_stake,
                    identity
                );
            }
        }

        if !offline_nodes.is_empty() {
            info!(
                "{}% of active stake is not visible in gossip",
                offline_stake * 100 / total_activated_stake
            );
            for (stake, identity) in offline_nodes {
                info!(
                    "    {}% - {}",
                    stake * 100 / total_activated_stake,
                    identity
                );
            }
        }
    }

    online_stake * 100 / total_activated_stake
}

// Cleanup anything that looks like an accounts append-vec
fn cleanup_accounts_path(account_path: &std::path::Path) {
    if std::fs::remove_dir_all(account_path).is_err() {
        warn!(
            "encountered error removing accounts path: {:?}",
            account_path
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_ledger::{create_new_tmp_ledger, genesis_utils::create_genesis_config_with_leader};
    use std::fs::remove_dir_all;

    #[test]
    fn validator_exit() {
        solana_logger::setup();
        let leader_keypair = Keypair::new();
        let leader_node = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());

        let validator_keypair = Keypair::new();
        let validator_node = Node::new_localhost_with_pubkey(&validator_keypair.pubkey());
        let genesis_config =
            create_genesis_config_with_leader(10_000, &leader_keypair.pubkey(), 1000)
                .genesis_config;
        let (validator_ledger_path, _blockhash) = create_new_tmp_ledger!(&genesis_config);

        let voting_keypair = Arc::new(Keypair::new());
        let config = ValidatorConfig {
            rpc_addrs: Some((
                validator_node.info.rpc,
                validator_node.info.rpc_pubsub,
                validator_node.info.rpc_banks,
            )),
            ..ValidatorConfig::default()
        };
        let validator = Validator::new(
            validator_node,
            &Arc::new(validator_keypair),
            &validator_ledger_path,
            &voting_keypair.pubkey(),
            vec![voting_keypair.clone()],
            Some(&leader_node.info),
            &config,
        );
        validator.close().unwrap();
        remove_dir_all(validator_ledger_path).unwrap();
    }

    #[test]
    fn test_backup_and_clear_blockstore() {
        use std::time::Instant;
        solana_logger::setup();
        use solana_ledger::get_tmp_ledger_path;
        use solana_ledger::{blockstore, entry};
        let blockstore_path = get_tmp_ledger_path!();
        {
            let blockstore = Blockstore::open(&blockstore_path).unwrap();

            let entries = entry::create_ticks(1, 0, Hash::default());

            info!("creating shreds");
            let mut last_print = Instant::now();
            for i in 1..10 {
                let shreds = blockstore::entries_to_test_shreds(entries.clone(), i, i - 1, true, 1);
                blockstore.insert_shreds(shreds, None, true).unwrap();
                if last_print.elapsed().as_millis() > 5000 {
                    info!("inserted {}", i);
                    last_print = Instant::now();
                }
            }
            drop(blockstore);

            backup_and_clear_blockstore(&blockstore_path, 5, 2);

            let blockstore = Blockstore::open(&blockstore_path).unwrap();
            assert!(blockstore.meta(4).unwrap().unwrap().next_slots.is_empty());
            for i in 5..10 {
                assert!(blockstore
                    .get_data_shreds_for_slot(i, 0)
                    .unwrap()
                    .is_empty());
            }
        }
    }

    #[test]
    fn validator_parallel_exit() {
        let leader_keypair = Keypair::new();
        let leader_node = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());

        let mut ledger_paths = vec![];
        let mut validators: Vec<Validator> = (0..2)
            .map(|_| {
                let validator_keypair = Keypair::new();
                let validator_node = Node::new_localhost_with_pubkey(&validator_keypair.pubkey());
                let genesis_config =
                    create_genesis_config_with_leader(10_000, &leader_keypair.pubkey(), 1000)
                        .genesis_config;
                let (validator_ledger_path, _blockhash) = create_new_tmp_ledger!(&genesis_config);
                ledger_paths.push(validator_ledger_path.clone());
                let vote_account_keypair = Keypair::new();
                let config = ValidatorConfig {
                    rpc_addrs: Some((
                        validator_node.info.rpc,
                        validator_node.info.rpc_pubsub,
                        validator_node.info.rpc_banks,
                    )),
                    ..ValidatorConfig::default()
                };
                Validator::new(
                    validator_node,
                    &Arc::new(validator_keypair),
                    &validator_ledger_path,
                    &vote_account_keypair.pubkey(),
                    vec![Arc::new(vote_account_keypair)],
                    Some(&leader_node.info),
                    &config,
                )
            })
            .collect();

        // Each validator can exit in parallel to speed many sequential calls to join`
        validators.iter_mut().for_each(|v| v.exit());
        // While join is called sequentially, the above exit call notified all the
        // validators to exit from all their threads
        validators.into_iter().for_each(|validator| {
            validator.join().unwrap();
        });

        for path in ledger_paths {
            remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn test_wait_for_supermajority() {
        solana_logger::setup();
        use solana_sdk::genesis_config::create_genesis_config;
        use solana_sdk::hash::hash;
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
            node_keypair,
        );

        let (genesis_config, _mint_keypair) = create_genesis_config(1);
        let bank = Arc::new(Bank::new(&genesis_config));
        let mut config = ValidatorConfig::default();
        let rpc_override_health_check = Arc::new(AtomicBool::new(false));
        assert!(!wait_for_supermajority(
            &config,
            &bank,
            &cluster_info,
            rpc_override_health_check.clone()
        ));

        // bank=0, wait=1, should fail
        config.wait_for_supermajority = Some(1);
        assert!(wait_for_supermajority(
            &config,
            &bank,
            &cluster_info,
            rpc_override_health_check.clone()
        ));

        // bank=1, wait=0, should pass, bank is past the wait slot
        let bank = Bank::new_from_parent(&bank, &Pubkey::default(), 1);
        config.wait_for_supermajority = Some(0);
        assert!(!wait_for_supermajority(
            &config,
            &bank,
            &cluster_info,
            rpc_override_health_check.clone()
        ));

        // bank=1, wait=1, equal, but bad hash provided
        config.wait_for_supermajority = Some(1);
        config.expected_bank_hash = Some(hash(&[1]));
        assert!(wait_for_supermajority(
            &config,
            &bank,
            &cluster_info,
            rpc_override_health_check
        ));
    }
}
