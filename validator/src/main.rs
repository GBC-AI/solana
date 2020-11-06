use clap::{
    crate_description, crate_name, value_t, value_t_or_exit, values_t, values_t_or_exit, App, Arg,
    ArgMatches,
};
use log::*;
use rand::{thread_rng, Rng};
use solana_clap_utils::{
    input_parsers::{keypair_of, keypairs_of, pubkey_of},
    input_validators::{
        is_keypair_or_ask_keyword, is_parsable, is_pubkey, is_pubkey_or_keypair, is_slot,
    },
    keypair::SKIP_SEED_PHRASE_VALIDATION_ARG,
};
use solana_client::rpc_client::RpcClient;
use solana_core::ledger_cleanup_service::CFG as LEDGER_CLEANUP_CFG;
use solana_core::{
    cluster_info::{ClusterInfo, Node, MINIMUM_VALIDATOR_PORT_RANGE_WIDTH, VALIDATOR_PORT_RANGE},
    contact_info::ContactInfo,
    gossip_service::GossipService,
    rpc::JsonRpcConfig,
    rpc_pubsub_service::PubSubConfig,
    validator::{Validator, ValidatorConfig},
};
use solana_download_utils::{download_genesis_if_missing, download_snapshot};
use solana_ledger::blockstore_db::BlockstoreRecoveryMode;
use solana_perf::recycler::enable_recycler_warming;
use solana_runtime::{
    bank_forks::{CompressionType, SnapshotConfig, SnapshotVersion},
    hardened_unpack::{unpack_genesis_archive, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE},
    snapshot_utils::get_highest_snapshot_archive_path,
};
use solana_sdk::{
    clock::Slot,
    commitment_config::CommitmentConfig,
    genesis_config::GenesisConfig,
    hash::Hash,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use std::{
    collections::HashSet,
    env,
    fs::{self, File},
    net::{SocketAddr, TcpListener, UdpSocket},
    path::{Path, PathBuf},
    process::exit,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{sleep, JoinHandle},
    time::{Duration, Instant},
};

fn port_validator(port: String) -> Result<(), String> {
    port.parse::<u16>()
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

fn port_range_validator(port_range: String) -> Result<(), String> {
    if let Some((start, end)) = solana_net_utils::parse_port_range(&port_range) {
        if end - start < MINIMUM_VALIDATOR_PORT_RANGE_WIDTH {
            Err(format!(
                "Port range is too small.  Try --dynamic-port-range {}-{}",
                start,
                start + MINIMUM_VALIDATOR_PORT_RANGE_WIDTH
            ))
        } else {
            Ok(())
        }
    } else {
        Err("Invalid port range".to_string())
    }
}

fn hash_validator(hash: String) -> Result<(), String> {
    Hash::from_str(&hash)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

fn is_trusted_validator(id: &Pubkey, trusted_validators: &Option<HashSet<Pubkey>>) -> bool {
    if let Some(trusted_validators) = trusted_validators {
        trusted_validators.contains(id)
    } else {
        false
    }
}

fn get_trusted_snapshot_hashes(
    cluster_info: &ClusterInfo,
    trusted_validators: &Option<HashSet<Pubkey>>,
) -> Option<HashSet<(Slot, Hash)>> {
    if let Some(trusted_validators) = trusted_validators {
        let mut trusted_snapshot_hashes = HashSet::new();
        for trusted_validator in trusted_validators {
            cluster_info.get_snapshot_hash_for_node(trusted_validator, |snapshot_hashes| {
                for snapshot_hash in snapshot_hashes {
                    trusted_snapshot_hashes.insert(*snapshot_hash);
                }
            });
        }
        Some(trusted_snapshot_hashes)
    } else {
        None
    }
}

fn start_gossip_node(
    identity_keypair: &Arc<Keypair>,
    entrypoint_gossip: &SocketAddr,
    gossip_addr: &SocketAddr,
    gossip_socket: UdpSocket,
    expected_shred_version: Option<u16>,
    gossip_validators: Option<HashSet<Pubkey>>,
) -> (Arc<ClusterInfo>, Arc<AtomicBool>, GossipService) {
    let cluster_info = ClusterInfo::new(
        ClusterInfo::gossip_contact_info(
            &identity_keypair.pubkey(),
            *gossip_addr,
            expected_shred_version.unwrap_or(0),
        ),
        identity_keypair.clone(),
    );
    cluster_info.set_entrypoint(ContactInfo::new_gossip_entry_point(entrypoint_gossip));
    let cluster_info = Arc::new(cluster_info);

    let gossip_exit_flag = Arc::new(AtomicBool::new(false));
    let gossip_service = GossipService::new(
        &cluster_info,
        None,
        gossip_socket,
        gossip_validators,
        &gossip_exit_flag,
    );
    (cluster_info, gossip_exit_flag, gossip_service)
}

fn get_rpc_node(
    cluster_info: &ClusterInfo,
    entrypoint_gossip: &SocketAddr,
    validator_config: &ValidatorConfig,
    blacklisted_rpc_nodes: &mut HashSet<Pubkey>,
    snapshot_not_required: bool,
    no_untrusted_rpc: bool,
    ledger_path: &std::path::Path,
) -> Option<(ContactInfo, Option<(Slot, Hash)>)> {
    let mut blacklist_timeout = Instant::now();
    let mut newer_cluster_snapshot_timeout = None;
    let mut retry_reason = None;
    loop {
        sleep(Duration::from_secs(1));
        info!("\n{}", cluster_info.rpc_info_trace());

        let shred_version = validator_config
            .expected_shred_version
            .unwrap_or_else(|| cluster_info.my_shred_version());
        if shred_version == 0 {
            if let Some(entrypoint) =
                cluster_info.lookup_contact_info_by_gossip_addr(entrypoint_gossip)
            {
                if entrypoint.shred_version == 0 {
                    eprintln!(
                        "Entrypoint shred version is zero.  Restart with --expected-shred-version"
                    );
                    exit(1);
                }
            }
            info!(
                "Waiting to adopt entrypoint shred version, contact info for {:?} not found...",
                entrypoint_gossip
            );
            continue;
        }

        info!(
            "Searching for an RPC service with shred version {}{}...",
            shred_version,
            retry_reason
                .as_ref()
                .map(|s| format!(" (Retrying: {})", s))
                .unwrap_or_default()
        );

        let rpc_peers = cluster_info
            .all_rpc_peers()
            .into_iter()
            .filter(|contact_info| contact_info.shred_version == shred_version)
            .collect::<Vec<_>>();
        let rpc_peers_total = rpc_peers.len();

        // Filter out blacklisted nodes
        let rpc_peers: Vec<_> = rpc_peers
            .into_iter()
            .filter(|rpc_peer| !blacklisted_rpc_nodes.contains(&rpc_peer.id))
            .collect();
        let rpc_peers_blacklisted = rpc_peers_total - rpc_peers.len();
        let rpc_peers_trusted = rpc_peers
            .iter()
            .filter(|rpc_peer| {
                is_trusted_validator(&rpc_peer.id, &validator_config.trusted_validators)
            })
            .count();

        info!(
            "Total {} RPC nodes found. {} trusted, {} blacklisted ",
            rpc_peers_total, rpc_peers_trusted, rpc_peers_blacklisted
        );

        if rpc_peers_blacklisted == rpc_peers_total {
            retry_reason = if blacklist_timeout.elapsed().as_secs() > 60 {
                // If all nodes are blacklisted and no additional nodes are discovered after 60 seconds,
                // remove the blacklist and try them all again
                blacklisted_rpc_nodes.clear();
                Some("Blacklist timeout expired".to_owned())
            } else {
                Some("Wait for trusted rpc peers".to_owned())
            };
            continue;
        }
        blacklist_timeout = Instant::now();

        let mut highest_snapshot_hash: Option<(Slot, Hash)> =
            get_highest_snapshot_archive_path(ledger_path)
                .map(|(_path, (slot, hash, _compression))| (slot, hash));
        let eligible_rpc_peers = if snapshot_not_required {
            rpc_peers
        } else {
            let trusted_snapshot_hashes =
                get_trusted_snapshot_hashes(&cluster_info, &validator_config.trusted_validators);

            let mut eligible_rpc_peers = vec![];

            for rpc_peer in rpc_peers.iter() {
                if no_untrusted_rpc
                    && !is_trusted_validator(&rpc_peer.id, &validator_config.trusted_validators)
                {
                    continue;
                }
                cluster_info.get_snapshot_hash_for_node(&rpc_peer.id, |snapshot_hashes| {
                    for snapshot_hash in snapshot_hashes {
                        if let Some(ref trusted_snapshot_hashes) = trusted_snapshot_hashes {
                            if !trusted_snapshot_hashes.contains(snapshot_hash) {
                                // Ignore all untrusted snapshot hashes
                                continue;
                            }
                        }

                        if highest_snapshot_hash.is_none()
                            || snapshot_hash.0 > highest_snapshot_hash.unwrap().0
                        {
                            // Found a higher snapshot, remove all nodes with a lower snapshot
                            eligible_rpc_peers.clear();
                            highest_snapshot_hash = Some(*snapshot_hash)
                        }

                        if Some(*snapshot_hash) == highest_snapshot_hash {
                            eligible_rpc_peers.push(rpc_peer.clone());
                        }
                    }
                });
            }

            match highest_snapshot_hash {
                None => {
                    assert!(eligible_rpc_peers.is_empty());
                }
                Some(highest_snapshot_hash) => {
                    if eligible_rpc_peers.is_empty() {
                        match newer_cluster_snapshot_timeout {
                            None => newer_cluster_snapshot_timeout = Some(Instant::now()),
                            Some(newer_cluster_snapshot_timeout) => {
                                if newer_cluster_snapshot_timeout.elapsed().as_secs() > 180 {
                                    warn!("giving up newer snapshot from the cluster");
                                    return None;
                                }
                            }
                        }
                        retry_reason = Some(format!(
                            "Wait for newer snapshot than local: {:?}",
                            highest_snapshot_hash
                        ));
                        continue;
                    }

                    info!(
                        "Highest available snapshot slot is {}, available from {} node{}: {:?}",
                        highest_snapshot_hash.0,
                        eligible_rpc_peers.len(),
                        if eligible_rpc_peers.len() > 1 {
                            "s"
                        } else {
                            ""
                        },
                        eligible_rpc_peers
                            .iter()
                            .map(|contact_info| contact_info.id)
                            .collect::<Vec<_>>()
                    );
                }
            }
            eligible_rpc_peers
        };

        if !eligible_rpc_peers.is_empty() {
            let contact_info =
                &eligible_rpc_peers[thread_rng().gen_range(0, eligible_rpc_peers.len())];
            return Some((contact_info.clone(), highest_snapshot_hash));
        } else {
            retry_reason = Some("No snapshots available".to_owned());
        }
    }
}

fn check_vote_account(
    rpc_client: &RpcClient,
    identity_pubkey: &Pubkey,
    vote_account_address: &Pubkey,
    authorized_voter_pubkeys: &[Pubkey],
) -> Result<(), String> {
    let vote_account = rpc_client
        .get_account_with_commitment(vote_account_address, CommitmentConfig::root())
        .map_err(|err| format!("failed to fetch vote account: {}", err.to_string()))?
        .value
        .ok_or_else(|| format!("vote account does not exist: {}", vote_account_address))?;

    if vote_account.owner != solana_vote_program::id() {
        return Err(format!(
            "not a vote account (owned by {}): {}",
            vote_account.owner, vote_account_address
        ));
    }

    let identity_account = rpc_client
        .get_account_with_commitment(identity_pubkey, CommitmentConfig::root())
        .map_err(|err| format!("failed to fetch identity account: {}", err.to_string()))?
        .value
        .ok_or_else(|| format!("identity account does not exist: {}", identity_pubkey))?;

    let vote_state = solana_vote_program::vote_state::VoteState::from(&vote_account);
    if let Some(vote_state) = vote_state {
        if vote_state.authorized_voters().is_empty() {
            return Err("Vote account not yet initialized".to_string());
        }

        if vote_state.node_pubkey != *identity_pubkey {
            return Err(format!(
                "vote account's identity ({}) does not match the validator's identity {}).",
                vote_state.node_pubkey, identity_pubkey
            ));
        }

        for (_, vote_account_authorized_voter_pubkey) in vote_state.authorized_voters().iter() {
            if !authorized_voter_pubkeys.contains(&vote_account_authorized_voter_pubkey) {
                return Err(format!(
                    "authorized voter {} not available",
                    vote_account_authorized_voter_pubkey
                ));
            }
        }
    } else {
        return Err(format!(
            "invalid vote account data for {}",
            vote_account_address
        ));
    }

    // Maybe we can calculate minimum voting fee; rather than 1 lamport
    if identity_account.lamports <= 1 {
        return Err(format!(
            "underfunded identity account ({}): only {} lamports available",
            identity_pubkey, identity_account.lamports
        ));
    }

    Ok(())
}

// This function is duplicated in ledger-tool/src/main.rs...
fn hardforks_of(matches: &ArgMatches<'_>, name: &str) -> Option<Vec<Slot>> {
    if matches.is_present(name) {
        Some(values_t_or_exit!(matches, name, Slot))
    } else {
        None
    }
}

fn validators_set(
    identity_pubkey: &Pubkey,
    matches: &ArgMatches<'_>,
    matches_name: &str,
    arg_name: &str,
) -> Option<HashSet<Pubkey>> {
    if matches.is_present(matches_name) {
        let validators_set: HashSet<_> = values_t_or_exit!(matches, matches_name, Pubkey)
            .into_iter()
            .collect();
        if validators_set.contains(identity_pubkey) {
            eprintln!(
                "The validator's identity pubkey cannot be a {}: {}",
                arg_name, identity_pubkey
            );
            exit(1);
        }
        Some(validators_set)
    } else {
        None
    }
}

fn check_genesis_hash(
    genesis_config: &GenesisConfig,
    expected_genesis_hash: Option<Hash>,
) -> Result<(), String> {
    let genesis_hash = genesis_config.hash();

    if let Some(expected_genesis_hash) = expected_genesis_hash {
        if expected_genesis_hash != genesis_hash {
            return Err(format!(
                "Genesis hash mismatch: expected {} but downloaded genesis hash is {}",
                expected_genesis_hash, genesis_hash,
            ));
        }
    }

    Ok(())
}

fn load_local_genesis(
    ledger_path: &std::path::Path,
    expected_genesis_hash: Option<Hash>,
) -> Result<GenesisConfig, String> {
    let existing_genesis = GenesisConfig::load(&ledger_path)
        .map_err(|err| format!("Failed to load genesis config: {}", err))?;
    check_genesis_hash(&existing_genesis, expected_genesis_hash)?;

    Ok(existing_genesis)
}

fn download_then_check_genesis_hash(
    rpc_addr: &SocketAddr,
    ledger_path: &std::path::Path,
    expected_genesis_hash: Option<Hash>,
    max_genesis_archive_unpacked_size: u64,
    no_genesis_fetch: bool,
) -> Result<Hash, String> {
    if no_genesis_fetch {
        let genesis_config = load_local_genesis(ledger_path, expected_genesis_hash)?;
        return Ok(genesis_config.hash());
    }

    let genesis_package = ledger_path.join("genesis.tar.bz2");
    let genesis_config =
        if let Ok(tmp_genesis_package) = download_genesis_if_missing(rpc_addr, &genesis_package) {
            unpack_genesis_archive(
                &tmp_genesis_package,
                &ledger_path,
                max_genesis_archive_unpacked_size,
            )
            .map_err(|err| format!("Failed to unpack downloaded genesis config: {}", err))?;

            let downloaded_genesis = GenesisConfig::load(&ledger_path)
                .map_err(|err| format!("Failed to load downloaded genesis config: {}", err))?;

            check_genesis_hash(&downloaded_genesis, expected_genesis_hash)?;
            std::fs::rename(tmp_genesis_package, genesis_package)
                .map_err(|err| format!("Unable to rename: {:?}", err))?;

            downloaded_genesis
        } else {
            load_local_genesis(ledger_path, expected_genesis_hash)?
        };

    Ok(genesis_config.hash())
}

fn is_snapshot_config_invalid(
    snapshot_interval_slots: u64,
    accounts_hash_interval_slots: u64,
) -> bool {
    snapshot_interval_slots != 0
        && (snapshot_interval_slots < accounts_hash_interval_slots
            || snapshot_interval_slots % accounts_hash_interval_slots != 0)
}

#[cfg(unix)]
fn redirect_stderr(filename: &str) {
    use std::{fs::OpenOptions, os::unix::io::AsRawFd};
    match OpenOptions::new()
        .write(true)
        .create(true)
        .append(true)
        .open(filename)
    {
        Ok(file) => unsafe {
            libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
        },
        Err(err) => eprintln!("Unable to open {}: {}", filename, err),
    }
}

fn start_logger(logfile: Option<String>) -> Option<JoinHandle<()>> {
    let logger_thread = match logfile {
        None => None,
        Some(logfile) => {
            #[cfg(unix)]
            {
                let signals = signal_hook::iterator::Signals::new(&[signal_hook::SIGUSR1])
                    .unwrap_or_else(|err| {
                        eprintln!("Unable to register SIGUSR1 handler: {:?}", err);
                        exit(1);
                    });

                redirect_stderr(&logfile);
                Some(std::thread::spawn(move || {
                    for signal in signals.forever() {
                        info!(
                            "received SIGUSR1 ({}), reopening log file: {:?}",
                            signal, logfile
                        );
                        redirect_stderr(&logfile);
                    }
                }))
            }
            #[cfg(not(unix))]
            {
                println!("logging to a file is not supported on this platform");
                ()
            }
        }
    };

    solana_logger::setup_with_default(
        &[
            "solana=info,solana_runtime::message_processor=error", /* info logging for all solana modules */
            "rpc=trace",   /* json_rpc request/response logging */
        ]
        .join(","),
    );

    logger_thread
}

fn verify_reachable_ports(
    node: &Node,
    cluster_entrypoint: &ContactInfo,
    validator_config: &ValidatorConfig,
) {
    let mut udp_sockets = vec![&node.sockets.gossip, &node.sockets.repair];

    if ContactInfo::is_valid_address(&node.info.serve_repair) {
        udp_sockets.push(&node.sockets.serve_repair);
    }
    if ContactInfo::is_valid_address(&node.info.tpu) {
        udp_sockets.extend(node.sockets.tpu.iter());
    }
    if ContactInfo::is_valid_address(&node.info.tpu_forwards) {
        udp_sockets.extend(node.sockets.tpu_forwards.iter());
    }
    if ContactInfo::is_valid_address(&node.info.tvu) {
        udp_sockets.extend(node.sockets.tvu.iter());
        udp_sockets.extend(node.sockets.broadcast.iter());
        udp_sockets.extend(node.sockets.retransmit_sockets.iter());
    }
    if ContactInfo::is_valid_address(&node.info.tvu_forwards) {
        udp_sockets.extend(node.sockets.tvu_forwards.iter());
    }

    let mut tcp_listeners = vec![];
    if let Some((rpc_addr, rpc_pubsub_addr, rpc_banks_addr)) = validator_config.rpc_addrs {
        for (purpose, bind_addr, public_addr) in &[
            ("RPC", rpc_addr, &node.info.rpc),
            ("RPC pubsub", rpc_pubsub_addr, &node.info.rpc_pubsub),
            ("RPC banks", rpc_banks_addr, &node.info.rpc_banks),
        ] {
            if ContactInfo::is_valid_address(&public_addr) {
                tcp_listeners.push((
                    bind_addr.port(),
                    TcpListener::bind(bind_addr).unwrap_or_else(|err| {
                        error!(
                            "Unable to bind to tcp {:?} for {}: {}",
                            bind_addr, purpose, err
                        );
                        exit(1);
                    }),
                ));
            }
        }
    }

    if let Some(ip_echo) = &node.sockets.ip_echo {
        let ip_echo = ip_echo.try_clone().expect("unable to clone tcp_listener");
        tcp_listeners.push((ip_echo.local_addr().unwrap().port(), ip_echo));
    }

    if !solana_net_utils::verify_reachable_ports(
        &cluster_entrypoint.gossip,
        tcp_listeners,
        &udp_sockets,
    ) {
        exit(1);
    }
}

struct RpcBootstrapConfig {
    no_genesis_fetch: bool,
    no_snapshot_fetch: bool,
    no_untrusted_rpc: bool,
    max_genesis_archive_unpacked_size: u64,
    no_check_vote_account: bool,
}

impl Default for RpcBootstrapConfig {
    fn default() -> Self {
        Self {
            no_genesis_fetch: true,
            no_snapshot_fetch: true,
            no_untrusted_rpc: true,
            max_genesis_archive_unpacked_size: MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            no_check_vote_account: true,
        }
    }
}

fn rpc_bootstrap(
    node: &Node,
    identity_keypair: &Arc<Keypair>,
    ledger_path: &Path,
    vote_account: &Pubkey,
    authorized_voter_keypairs: &[Arc<Keypair>],
    cluster_entrypoint: &ContactInfo,
    validator_config: &mut ValidatorConfig,
    bootstrap_config: RpcBootstrapConfig,
    no_port_check: bool,
) {
    if !no_port_check {
        verify_reachable_ports(&node, cluster_entrypoint, &validator_config);
    }

    if bootstrap_config.no_genesis_fetch && bootstrap_config.no_snapshot_fetch {
        return;
    }

    let mut blacklisted_rpc_nodes = HashSet::new();
    let mut gossip = None;
    loop {
        if gossip.is_none() {
            gossip = Some(start_gossip_node(
                &identity_keypair,
                &cluster_entrypoint.gossip,
                &node.info.gossip,
                node.sockets.gossip.try_clone().unwrap(),
                validator_config.expected_shred_version,
                validator_config.gossip_validators.clone(),
            ));
        }

        let rpc_node_details = get_rpc_node(
            &gossip.as_ref().unwrap().0,
            &cluster_entrypoint.gossip,
            &validator_config,
            &mut blacklisted_rpc_nodes,
            bootstrap_config.no_snapshot_fetch,
            bootstrap_config.no_untrusted_rpc,
            ledger_path,
        );
        if rpc_node_details.is_none() {
            return;
        }
        let (rpc_contact_info, snapshot_hash) = rpc_node_details.unwrap();

        info!(
            "Using RPC service from node {}: {:?}",
            rpc_contact_info.id, rpc_contact_info.rpc
        );
        let rpc_client = RpcClient::new_socket(rpc_contact_info.rpc);

        let result = match rpc_client.get_version() {
            Ok(rpc_version) => {
                info!("RPC node version: {}", rpc_version.solana_core);
                Ok(())
            }
            Err(err) => Err(format!("Failed to get RPC node version: {}", err)),
        }
        .and_then(|_| {
            let genesis_hash = download_then_check_genesis_hash(
                &rpc_contact_info.rpc,
                &ledger_path,
                validator_config.expected_genesis_hash,
                bootstrap_config.max_genesis_archive_unpacked_size,
                bootstrap_config.no_genesis_fetch,
            );

            if let Ok(genesis_hash) = genesis_hash {
                if validator_config.expected_genesis_hash.is_none() {
                    info!("Expected genesis hash set to {}", genesis_hash);
                    validator_config.expected_genesis_hash = Some(genesis_hash);
                }
            }

            if let Some(expected_genesis_hash) = validator_config.expected_genesis_hash {
                // Sanity check that the RPC node is using the expected genesis hash before
                // downloading a snapshot from it
                let rpc_genesis_hash = rpc_client
                    .get_genesis_hash()
                    .map_err(|err| format!("Failed to get genesis hash: {}", err))?;

                if expected_genesis_hash != rpc_genesis_hash {
                    return Err(format!(
                        "Genesis hash mismatch: expected {} but RPC node genesis hash is {}",
                        expected_genesis_hash, rpc_genesis_hash
                    ));
                }
            }

            if let Some(snapshot_hash) = snapshot_hash {
                rpc_client
                    .get_slot_with_commitment(CommitmentConfig::root())
                    .map_err(|err| format!("Failed to get RPC node slot: {}", err))
                    .and_then(|slot| {
                        info!("RPC node root slot: {}", slot);
                        let (_cluster_info, gossip_exit_flag, gossip_service) =
                            gossip.take().unwrap();
                        gossip_exit_flag.store(true, Ordering::Relaxed);
                        let ret =
                            download_snapshot(&rpc_contact_info.rpc, &ledger_path, snapshot_hash);
                        gossip_service.join().unwrap();
                        ret
                    })
            } else {
                Ok(())
            }
        })
        .map(|_| {
            if !validator_config.voting_disabled && !bootstrap_config.no_check_vote_account {
                check_vote_account(
                    &rpc_client,
                    &identity_keypair.pubkey(),
                    &vote_account,
                    &authorized_voter_keypairs
                        .iter()
                        .map(|k| k.pubkey())
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|err| {
                    // Consider failures here to be more likely due to user error (eg,
                    // incorrect `solana-validator` command-line arguments) rather than the
                    // RPC node failing.
                    //
                    // Power users can always use the `--no-check-vote-account` option to
                    // bypass this check entirely
                    error!("{}", err);
                    exit(1);
                });
            }
        });

        if result.is_ok() {
            break;
        }
        warn!("{}", result.unwrap_err());

        if let Some(ref trusted_validators) = validator_config.trusted_validators {
            if trusted_validators.contains(&rpc_contact_info.id) {
                continue; // Never blacklist a trusted node
            }
        }

        info!(
            "Excluding {} as a future RPC candidate",
            rpc_contact_info.id
        );
        blacklisted_rpc_nodes.insert(rpc_contact_info.id);
    }
    if let Some((_cluster_info, gossip_exit_flag, gossip_service)) = gossip.take() {
        gossip_exit_flag.store(true, Ordering::Relaxed);
        gossip_service.join().unwrap();
    }
}

fn create_validator(
    node: Node,
    identity_keypair: &Arc<Keypair>,
    ledger_path: &Path,
    vote_account: &Pubkey,
    authorized_voter_keypairs: Vec<Arc<Keypair>>,
    cluster_entrypoint: Option<ContactInfo>,
    mut validator_config: ValidatorConfig,
    rpc_bootstrap_config: RpcBootstrapConfig,
    no_port_check: bool,
) -> Validator {
    if validator_config.cuda {
        solana_perf::perf_libs::init_cuda();
        enable_recycler_warming();
    }
    solana_ledger::entry::init_poh();

    if let Some(ref cluster_entrypoint) = cluster_entrypoint {
        rpc_bootstrap(
            &node,
            &identity_keypair,
            &ledger_path,
            &vote_account,
            &authorized_voter_keypairs,
            cluster_entrypoint,
            &mut validator_config,
            rpc_bootstrap_config,
            no_port_check,
        );
    }

    Validator::new(
        node,
        &identity_keypair,
        &ledger_path,
        &vote_account,
        authorized_voter_keypairs,
        cluster_entrypoint.as_ref(),
        &validator_config,
    )
}

pub fn main() {
    let default_dynamic_port_range =
        &format!("{}-{}", VALIDATOR_PORT_RANGE.0, VALIDATOR_PORT_RANGE.1);
    let default_genesis_archive_unpacked_size = &MAX_GENESIS_ARCHIVE_UNPACKED_SIZE.to_string();
    let default_rpc_pubsub_max_connections = PubSubConfig::default().max_connections.to_string();
    let default_rpc_pubsub_max_fragment_size =
        PubSubConfig::default().max_fragment_size.to_string();
    let default_rpc_pubsub_max_in_buffer_capacity =
        PubSubConfig::default().max_in_buffer_capacity.to_string();
    let default_rpc_pubsub_max_out_buffer_capacity =
        PubSubConfig::default().max_out_buffer_capacity.to_string();

    let matches = App::new(crate_name!()).about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::with_name(SKIP_SEED_PHRASE_VALIDATION_ARG.name)
                .long(SKIP_SEED_PHRASE_VALIDATION_ARG.long)
                .help(SKIP_SEED_PHRASE_VALIDATION_ARG.help),
        )
        .arg(
            Arg::with_name("identity")
                .short("i")
                .long("identity")
                .value_name("PATH")
                .takes_value(true)
                .validator(is_keypair_or_ask_keyword)
                .help("Validator identity keypair"),
        )
        .arg(
            Arg::with_name("authorized_voter_keypairs")
                .long("authorized-voter")
                .value_name("PATH")
                .takes_value(true)
                .validator(is_keypair_or_ask_keyword)
                .requires("vote_account")
                .multiple(true)
                .help("Include an additional authorized voter keypair. \
                       May be specified multiple times. \
                       [default: the --identity keypair]"),
        )
        .arg(
            Arg::with_name("vote_account")
                .long("vote-account")
                .value_name("PUBKEY")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .requires("identity")
                .help("Validator vote account public key.  \
                       If unspecified voting will be disabled. \
                       The authorized voter for the account must either be the \
                       --identity keypair or with the --authorized-voter argument")
        )
        .arg(
            Arg::with_name("init_complete_file")
                .long("init-complete-file")
                .value_name("FILE")
                .takes_value(true)
                .help("Create this file if it doesn't already exist \
                       once node initialization is complete"),
        )
        .arg(
            Arg::with_name("ledger_path")
                .short("l")
                .long("ledger")
                .value_name("DIR")
                .takes_value(true)
                .required(true)
                .help("Use DIR as persistent ledger location"),
        )
        .arg(
            Arg::with_name("entrypoint")
                .short("n")
                .long("entrypoint")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(solana_net_utils::is_host_port)
                .help("Rendezvous with the cluster at this gossip entrypoint"),
        )
        .arg(
            Arg::with_name("no_snapshot_fetch")
                .long("no-snapshot-fetch")
                .takes_value(false)
                .help("Do not attempt to fetch a snapshot from the cluster, \
                      start from a local snapshot if present"),
        )
        .arg(
            Arg::with_name("no_genesis_fetch")
                .long("no-genesis-fetch")
                .takes_value(false)
                .help("Do not fetch genesis from the cluster"),
        )
        .arg(
            Arg::with_name("no_voting")
                .long("no-voting")
                .takes_value(false)
                .help("Launch node without voting"),
        )
        .arg(
            Arg::with_name("no_check_vote_account")
                .long("no-check-vote-account")
                .takes_value(false)
                .conflicts_with("no_voting")
                .requires("entrypoint")
                .help("Skip the RPC vote account sanity check")
        )
        .arg(
            Arg::with_name("restricted_repair_only_mode")
                .long("restricted-repair-only-mode")
                .takes_value(false)
                .help("Do not publish the Gossip, TPU, TVU or Repair Service ports causing \
                       the validator to operate in a limited capacity that reduces its \
                       exposure to the rest of the cluster. \
                       \
                       The --no-voting flag is implicit when this flag is enabled \
                      "),
        )
        .arg(
            Arg::with_name("dev_halt_at_slot")
                .long("dev-halt-at-slot")
                .value_name("SLOT")
                .validator(is_slot)
                .takes_value(true)
                .help("Halt the validator when it reaches the given slot"),
        )
        .arg(
            Arg::with_name("rpc_port")
                .long("rpc-port")
                .value_name("PORT")
                .takes_value(true)
                .validator(port_validator)
                .help("Use this port for JSON RPC, the next port for the RPC websocket, and then third port for the RPC banks API"),
        )
        .arg(
            Arg::with_name("private_rpc")
                .long("--private-rpc")
                .takes_value(false)
                .help("Do not publish the RPC port for use by other nodes")
        )
        .arg(
            Arg::with_name("no_port_check")
                .long("--no-port-check")
                .takes_value(false)
                .help("Do not perform TCP/UDP reachable port checks at start-up")
        )
        .arg(
            Arg::with_name("enable_rpc_exit")
                .long("enable-rpc-exit")
                .takes_value(false)
                .help("Enable the JSON RPC 'validatorExit' API. \
                       Only enable in a debug environment"),
        )
        .arg(
            Arg::with_name("enable_rpc_set_log_filter")
                .long("enable-rpc-set-log-filter")
                .takes_value(false)
                .help("Enable the JSON RPC 'setLogFilter' API. \
                       Only enable in a debug environment"),
        )
        .arg(
            Arg::with_name("enable_rpc_transaction_history")
                .long("enable-rpc-transaction-history")
                .takes_value(false)
                .help("Enable historical transaction info over JSON RPC, \
                       including the 'getConfirmedBlock' API.  \
                       This will cause an increase in disk usage and IOPS"),
        )
        .arg(
            Arg::with_name("enable_rpc_bigtable_ledger_storage")
                .long("enable-rpc-bigtable-ledger-storage")
                .requires("enable_rpc_transaction_history")
                .takes_value(false)
                .help("Fetch historical transaction info from a BigTable instance \
                       as a fallback to local ledger data"),
        )
        .arg(
            Arg::with_name("enable_bigtable_ledger_upload")
                .long("enable-bigtable-ledger-upload")
                .requires("enable_rpc_transaction_history")
                .takes_value(false)
                .help("Upload new confirmed blocks into a BigTable instance"),
        )
        .arg(
            Arg::with_name("health_check_slot_distance")
                .long("health-check-slot-distance")
                .value_name("SLOT_DISTANCE")
                .takes_value(true)
                .default_value("150")
                .help("If --trusted-validators are specified, report this validator healthy \
                       if its latest account hash is no further behind than this number of \
                       slots from the latest trusted validator account hash. \
                       If no --trusted-validators are specified, the validator will always \
                       report itself to be healthy")
        )
        .arg(
            Arg::with_name("rpc_faucet_addr")
                .long("rpc-faucet-address")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(solana_net_utils::is_host_port)
                .help("Enable the JSON RPC 'requestAirdrop' API with this faucet address."),
        )
        .arg(
            Arg::with_name("signer_addr")
                .long("vote-signer-address")
                .value_name("HOST:PORT")
                .takes_value(true)
                .hidden(true) // Don't document this argument to discourage its use
                .validator(solana_net_utils::is_host_port)
                .help("Rendezvous with the vote signer at this RPC end point"),
        )
        .arg(
            Arg::with_name("account_paths")
                .long("accounts")
                .value_name("PATHS")
                .takes_value(true)
                .help("Comma separated persistent accounts location"),
        )
        .arg(
            Arg::with_name("gossip_port")
                .long("gossip-port")
                .value_name("PORT")
                .takes_value(true)
                .help("Gossip port number for the node"),
        )
        .arg(
            Arg::with_name("gossip_host")
                .long("gossip-host")
                .value_name("HOST")
                .takes_value(true)
                .conflicts_with("entrypoint")
                .validator(solana_net_utils::is_host)
                .help("IP address for the node to advertise in gossip when \
                      --entrypoint is not provided [default: 127.0.0.1]"),
        )
        .arg(
            Arg::with_name("public_rpc_addr")
                .long("public-rpc-address")
                .value_name("HOST:PORT")
                .takes_value(true)
                .conflicts_with("private_rpc")
                .validator(solana_net_utils::is_host_port)
                .help("RPC address for the node to advertise publicly in gossip. \
                      Useful for nodes running behind a load balancer or proxy \
                      [default: use --rpc-bind-address / --rpc-port]"),
        )
        .arg(
            Arg::with_name("dynamic_port_range")
                .long("dynamic-port-range")
                .value_name("MIN_PORT-MAX_PORT")
                .takes_value(true)
                .default_value(default_dynamic_port_range)
                .validator(port_range_validator)
                .help("Range to use for dynamically assigned ports"),
        )
        .arg(
            Arg::with_name("snapshot_interval_slots")
                .long("snapshot-interval-slots")
                .value_name("SNAPSHOT_INTERVAL_SLOTS")
                .takes_value(true)
                .default_value("100")
                .help("Number of slots between generating snapshots, \
                      0 to disable snapshots"),
        )
        .arg(
            Arg::with_name("accounts_hash_interval_slots")
                .long("accounts-hash-slots")
                .value_name("ACCOUNTS_HASH_INTERVAL_SLOTS")
                .takes_value(true)
                .default_value("100")
                .help("Number of slots between generating accounts hash."),
        )
        .arg(
            Arg::with_name("snapshot_version")
                .long("snapshot-version")
                .value_name("SNAPSHOT_VERSION")
                .validator(is_parsable::<SnapshotVersion>)
                .takes_value(true)
                .default_value(SnapshotVersion::default().into())
                .help("Output snapshot version"),
        )
        .arg(
            Arg::with_name("limit_ledger_size")
                .long("limit-ledger-size")
                .value_name("SHRED_COUNT")
                .takes_value(true)
                .min_values(0)
                .max_values(1)
                /* .default_value() intentionally not used here! */
                .help("Keep this amount of shreds in root slots."),
        )
        .arg(
            Arg::with_name("skip_poh_verify")
                .long("skip-poh-verify")
                .takes_value(false)
                .help("Skip ledger verification at node bootup"),
        )
        .arg(
            Arg::with_name("cuda")
                .long("cuda")
                .takes_value(false)
                .help("Use CUDA"),
        )
        .arg(
            clap::Arg::with_name("require_tower")
                .long("require-tower")
                .takes_value(false)
                .help("Refuse to start if saved tower state is not found"),
        )
        .arg(
            Arg::with_name("expected_genesis_hash")
                .long("expected-genesis-hash")
                .value_name("HASH")
                .takes_value(true)
                .validator(hash_validator)
                .help("Require the genesis have this hash"),
        )
        .arg(
            Arg::with_name("expected_bank_hash")
                .long("expected-bank-hash")
                .value_name("HASH")
                .takes_value(true)
                .validator(hash_validator)
                .help("When wait-for-supermajority <x>, require the bank at <x> to have this hash"),
        )
        .arg(
            Arg::with_name("expected_shred_version")
                .long("expected-shred-version")
                .value_name("VERSION")
                .takes_value(true)
                .help("Require the shred version be this value"),
        )
        .arg(
            Arg::with_name("logfile")
                .short("o")
                .long("log")
                .value_name("FILE")
                .takes_value(true)
                .help("Redirect logging to the specified file, '-' for standard error. \
                       Sending the SIGUSR1 signal to the validator process will cause it \
                       to re-open the log file"),
        )
        .arg(
            Arg::with_name("wait_for_supermajority")
                .long("wait-for-supermajority")
                .requires("expected_bank_hash")
                .value_name("SLOT")
                .validator(is_slot)
                .help("After processing the ledger and the next slot is SLOT, wait until a \
                       supermajority of stake is visible on gossip before starting PoH"),
        )
        .arg(
            Arg::with_name("hard_forks")
                .long("hard-fork")
                .value_name("SLOT")
                .validator(is_slot)
                .multiple(true)
                .takes_value(true)
                .help("Add a hard fork at this slot"),
        )
        .arg(
            Arg::with_name("trusted_validators")
                .long("trusted-validator")
                .validator(is_pubkey)
                .value_name("PUBKEY")
                .multiple(true)
                .takes_value(true)
                .help("A snapshot hash must be published in gossip by this validator to be accepted. \
                       May be specified multiple times. If unspecified any snapshot hash will be accepted"),
        )
        .arg(
            Arg::with_name("debug_key")
                .long("debug-key")
                .validator(is_pubkey)
                .value_name("PUBKEY")
                .multiple(true)
                .takes_value(true)
                .help("Log when transactions are processed which reference a given key."),
        )
        .arg(
            Arg::with_name("no_untrusted_rpc")
                .long("no-untrusted-rpc")
                .takes_value(false)
                .help("Use the RPC service of trusted validators only")
        )
        .arg(
            Arg::with_name("repair_validators")
                .long("repair-validator")
                .validator(is_pubkey)
                .value_name("PUBKEY")
                .multiple(true)
                .takes_value(true)
                .help("A list of validators to request repairs from. If specified, repair will not \
                       request from validators outside this set [default: all validators]")
        )
        .arg(
            Arg::with_name("gossip_validators")
                .long("gossip-validator")
                .validator(is_pubkey)
                .value_name("PUBKEY")
                .multiple(true)
                .takes_value(true)
                .help("A list of validators to gossip with.  If specified, gossip \
                      will not pull/pull from from validators outside this set. \
                      [default: all validators]")
        )
        .arg(
            Arg::with_name("no_rocksdb_compaction")
                .long("no-rocksdb-compaction")
                .takes_value(false)
                .help("Disable manual compaction of the ledger database. May increase storage requirements.")
        )
        .arg(
            Arg::with_name("bind_address")
                .long("bind-address")
                .value_name("HOST")
                .takes_value(true)
                .validator(solana_net_utils::is_host)
                .default_value("0.0.0.0")
                .help("IP address to bind the validator ports"),
        )
        .arg(
            Arg::with_name("rpc_bind_address")
                .long("rpc-bind-address")
                .value_name("HOST")
                .takes_value(true)
                .validator(solana_net_utils::is_host)
                .help("IP address to bind the RPC port [default: use --bind-address]"),
        )
        .arg(
            Arg::with_name("rpc_pubsub_max_connections")
                .long("rpc-pubsub-max-connections")
                .value_name("NUMBER")
                .takes_value(true)
                .validator(is_parsable::<usize>)
                .default_value(&default_rpc_pubsub_max_connections)
                .help("The maximum number of connections that RPC PubSub will support. \
                       This is a hard limit and no new connections beyond this limit can \
                       be made until an old connection is dropped."),
        )
        .arg(
            Arg::with_name("rpc_pubsub_max_fragment_size")
                .long("rpc-pubsub-max-fragment-size")
                .value_name("BYTES")
                .takes_value(true)
                .validator(is_parsable::<usize>)
                .default_value(&default_rpc_pubsub_max_fragment_size)
                .help("The maximum length in bytes of acceptable incoming frames. Messages longer \
                       than this will be rejected."),
        )
        .arg(
            Arg::with_name("rpc_pubsub_max_in_buffer_capacity")
                .long("rpc-pubsub-max-in-buffer-capacity")
                .value_name("BYTES")
                .takes_value(true)
                .validator(is_parsable::<usize>)
                .default_value(&default_rpc_pubsub_max_in_buffer_capacity)
                .help("The maximum size in bytes to which the incoming websocket buffer can grow."),
        )
        .arg(
            Arg::with_name("rpc_pubsub_max_out_buffer_capacity")
                .long("rpc-pubsub-max-out-buffer-capacity")
                .value_name("BYTES")
                .takes_value(true)
                .validator(is_parsable::<usize>)
                .default_value(&default_rpc_pubsub_max_out_buffer_capacity)
                .help("The maximum size in bytes to which the outgoing websocket buffer can grow."),
        )
        .arg(
            Arg::with_name("halt_on_trusted_validators_accounts_hash_mismatch")
                .long("halt-on-trusted-validators-accounts-hash-mismatch")
                .requires("trusted_validators")
                .takes_value(false)
                .help("Abort the validator if a bank hash mismatch is detected within trusted validator set"),
        )
        .arg(
            Arg::with_name("frozen_accounts")
                .long("frozen-account")
                .validator(is_pubkey)
                .value_name("PUBKEY")
                .multiple(true)
                .takes_value(true)
                .help("Freeze the specified account.  This will cause the validator to \
                       intentionally crash should any transaction modify the frozen account in any way \
                       other than increasing the account balance"),
        )
        .arg(
            Arg::with_name("snapshot_compression")
                .long("snapshot-compression")
                .possible_values(&["bz2", "gzip", "zstd", "none"])
                .default_value("zstd")
                .value_name("COMPRESSION_TYPE")
                .takes_value(true)
                .help("Type of snapshot compression to use."),
        )
        .arg(
            Arg::with_name("max_genesis_archive_unpacked_size")
                .long("max-genesis-archive-unpacked-size")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(&default_genesis_archive_unpacked_size)
                .help(
                    "maximum total uncompressed file size of downloaded genesis archive",
                ),
        )
        .arg(
            Arg::with_name("wal_recovery_mode")
                .long("wal-recovery-mode")
                .value_name("MODE")
                .takes_value(true)
                .possible_values(&[
                    "tolerate_corrupted_tail_records",
                    "absolute_consistency",
                    "point_in_time",
                    "skip_any_corrupted_record"])
                .help(
                    "Mode to recovery the ledger db write ahead log."
                ),
        )
        .get_matches();

    let identity_keypair = Arc::new(keypair_of(&matches, "identity").unwrap_or_else(Keypair::new));

    let authorized_voter_keypairs = keypairs_of(&matches, "authorized_voter_keypairs")
        .map(|keypairs| keypairs.into_iter().map(Arc::new).collect())
        .unwrap_or_else(|| vec![identity_keypair.clone()]);

    let ledger_path = PathBuf::from(matches.value_of("ledger_path").unwrap());
    let init_complete_file = matches.value_of("init_complete_file");

    let rpc_bootstrap_config = RpcBootstrapConfig {
        no_genesis_fetch: matches.is_present("no_genesis_fetch"),
        no_snapshot_fetch: matches.is_present("no_snapshot_fetch"),
        no_check_vote_account: matches.is_present("no_check_vote_account"),
        no_untrusted_rpc: matches.is_present("no_untrusted_rpc"),
        max_genesis_archive_unpacked_size: value_t_or_exit!(
            matches,
            "max_genesis_archive_unpacked_size",
            u64
        ),
    };

    let private_rpc = matches.is_present("private_rpc");
    let no_port_check = matches.is_present("no_port_check");
    let no_rocksdb_compaction = matches.is_present("no_rocksdb_compaction");
    let wal_recovery_mode = matches
        .value_of("wal_recovery_mode")
        .map(BlockstoreRecoveryMode::from);

    // Canonicalize ledger path to avoid issues with symlink creation
    let _ = fs::create_dir_all(&ledger_path);
    let ledger_path = fs::canonicalize(&ledger_path).unwrap_or_else(|err| {
        eprintln!("Unable to access ledger path: {:?}", err);
        exit(1);
    });

    let debug_keys: Option<Arc<HashSet<_>>> = if matches.is_present("debug_key") {
        Some(Arc::new(
            values_t_or_exit!(matches, "debug_key", Pubkey)
                .into_iter()
                .collect(),
        ))
    } else {
        None
    };

    let trusted_validators = validators_set(
        &identity_keypair.pubkey(),
        &matches,
        "trusted_validators",
        "--trusted-validator",
    );
    let repair_validators = validators_set(
        &identity_keypair.pubkey(),
        &matches,
        "repair_validators",
        "--repair-validator",
    );
    let gossip_validators = validators_set(
        &identity_keypair.pubkey(),
        &matches,
        "gossip_validators",
        "--gossip-validator",
    );

    let bind_address = solana_net_utils::parse_host(matches.value_of("bind_address").unwrap())
        .expect("invalid bind_address");
    let rpc_bind_address = if matches.is_present("rpc_bind_address") {
        solana_net_utils::parse_host(matches.value_of("rpc_bind_address").unwrap())
            .expect("invalid rpc_bind_address")
    } else {
        bind_address
    };

    let restricted_repair_only_mode = matches.is_present("restricted_repair_only_mode");
    let mut validator_config = ValidatorConfig {
        require_tower: matches.is_present("require_tower"),
        dev_halt_at_slot: value_t!(matches, "dev_halt_at_slot", Slot).ok(),
        cuda: matches.is_present("cuda"),
        expected_genesis_hash: matches
            .value_of("expected_genesis_hash")
            .map(|s| Hash::from_str(&s).unwrap()),
        expected_bank_hash: matches
            .value_of("expected_bank_hash")
            .map(|s| Hash::from_str(&s).unwrap()),
        expected_shred_version: value_t!(matches, "expected_shred_version", u16).ok(),
        new_hard_forks: hardforks_of(&matches, "hard_forks"),
        rpc_config: JsonRpcConfig {
            enable_validator_exit: matches.is_present("enable_rpc_exit"),
            enable_set_log_filter: matches.is_present("enable_rpc_set_log_filter"),
            enable_rpc_transaction_history: matches.is_present("enable_rpc_transaction_history"),
            enable_bigtable_ledger_storage: matches
                .is_present("enable_rpc_bigtable_ledger_storage"),
            enable_bigtable_ledger_upload: matches.is_present("enable_bigtable_ledger_upload"),
            identity_pubkey: identity_keypair.pubkey(),
            faucet_addr: matches.value_of("rpc_faucet_addr").map(|address| {
                solana_net_utils::parse_host_port(address).expect("failed to parse faucet address")
            }),
            health_check_slot_distance: value_t_or_exit!(
                matches,
                "health_check_slot_distance",
                u64
            ),
        },
        rpc_addrs: value_t!(matches, "rpc_port", u16).ok().map(|rpc_port| {
            (
                SocketAddr::new(rpc_bind_address, rpc_port),
                SocketAddr::new(rpc_bind_address, rpc_port + 1),
                // +2 is skipped to avoid a conflict with the websocket port (which is +2) in web3.js
                // This odd port shifting is tracked at https://github.com/solana-labs/solana/issues/12250
                SocketAddr::new(rpc_bind_address, rpc_port + 3),
            )
        }),
        pubsub_config: PubSubConfig {
            max_connections: value_t_or_exit!(matches, "rpc_pubsub_max_connections", usize),
            max_fragment_size: value_t_or_exit!(matches, "rpc_pubsub_max_fragment_size", usize),
            max_in_buffer_capacity: value_t_or_exit!(
                matches,
                "rpc_pubsub_max_in_buffer_capacity",
                usize
            ),
            max_out_buffer_capacity: value_t_or_exit!(
                matches,
                "rpc_pubsub_max_out_buffer_capacity",
                usize
            ),
        },
        voting_disabled: matches.is_present("no_voting") || restricted_repair_only_mode,
        wait_for_supermajority: value_t!(matches, "wait_for_supermajority", Slot).ok(),
        trusted_validators,
        repair_validators,
        gossip_validators,
        frozen_accounts: values_t!(matches, "frozen_accounts", Pubkey).unwrap_or_default(),
        no_rocksdb_compaction,
        wal_recovery_mode,
        poh_verify: !matches.is_present("skip_poh_verify"),
        debug_keys,
        ..ValidatorConfig::default()
    };

    let vote_account = pubkey_of(&matches, "vote_account").unwrap_or_else(|| {
        if !validator_config.voting_disabled {
            warn!("--vote-account not specified, validator will not vote");
            validator_config.voting_disabled = true;
        }
        Keypair::new().pubkey()
    });

    let dynamic_port_range =
        solana_net_utils::parse_port_range(matches.value_of("dynamic_port_range").unwrap())
            .expect("invalid dynamic_port_range");

    let account_paths = if let Some(account_paths) = matches.value_of("account_paths") {
        account_paths.split(',').map(PathBuf::from).collect()
    } else {
        vec![ledger_path.join("accounts")]
    };

    // Create and canonicalize account paths to avoid issues with symlink creation
    validator_config.account_paths = account_paths
        .into_iter()
        .map(|account_path| {
            match fs::create_dir_all(&account_path).and_then(|_| fs::canonicalize(&account_path)) {
                Ok(account_path) => account_path,
                Err(err) => {
                    eprintln!(
                        "Unable to access account path: {:?}, err: {:?}",
                        account_path, err
                    );
                    exit(1);
                }
            }
        })
        .collect();

    let snapshot_interval_slots = value_t_or_exit!(matches, "snapshot_interval_slots", u64);
    let snapshot_path = ledger_path.join("snapshot");
    fs::create_dir_all(&snapshot_path).unwrap_or_else(|err| {
        eprintln!(
            "Failed to create snapshots directory {:?}: {}",
            snapshot_path, err
        );
        exit(1);
    });

    let snapshot_compression = {
        let compression_str = value_t_or_exit!(matches, "snapshot_compression", String);
        match compression_str.as_str() {
            "bz2" => CompressionType::Bzip2,
            "gzip" => CompressionType::Gzip,
            "zstd" => CompressionType::Zstd,
            "none" => CompressionType::NoCompression,
            _ => panic!("Compression type not recognized: {}", compression_str),
        }
    };

    let snapshot_version =
        matches
            .value_of("snapshot_version")
            .map_or(SnapshotVersion::default(), |s| {
                s.parse::<SnapshotVersion>().unwrap_or_else(|err| {
                    eprintln!("Error: {}", err);
                    exit(1)
                })
            });
    validator_config.snapshot_config = Some(SnapshotConfig {
        snapshot_interval_slots: if snapshot_interval_slots > 0 {
            snapshot_interval_slots
        } else {
            std::u64::MAX
        },
        snapshot_path,
        snapshot_package_output_path: ledger_path.clone(),
        compression: snapshot_compression,
        snapshot_version,
    });

    validator_config.accounts_hash_interval_slots =
        value_t_or_exit!(matches, "accounts_hash_interval_slots", u64);
    if validator_config.accounts_hash_interval_slots == 0 {
        eprintln!("Accounts hash interval should not be 0.");
        exit(1);
    }
    if is_snapshot_config_invalid(
        snapshot_interval_slots,
        validator_config.accounts_hash_interval_slots,
    ) {
        eprintln!("Invalid snapshot interval provided ({}), must be a multiple of accounts_hash_interval_slots ({})",
            snapshot_interval_slots,
            validator_config.accounts_hash_interval_slots,
        );
        exit(1);
    }

    if matches.is_present("limit_ledger_size") {
        let limit_ledger_size = match matches.value_of("limit_ledger_size") {
            Some(_) => value_t_or_exit!(matches, "limit_ledger_size", u64),
            None => LEDGER_CLEANUP_CFG.DEFAULT_MAX_LEDGER_SHREDS,
        };
        if limit_ledger_size < LEDGER_CLEANUP_CFG.DEFAULT_MIN_MAX_LEDGER_SHREDS {
            eprintln!(
                "The provided --limit-ledger-size value was too small, the minimum value is {}",
                LEDGER_CLEANUP_CFG.DEFAULT_MIN_MAX_LEDGER_SHREDS
            );
            exit(1);
        }
        validator_config.max_ledger_shreds = Some(limit_ledger_size);
    }

    if matches.is_present("halt_on_trusted_validators_accounts_hash_mismatch") {
        validator_config.halt_on_trusted_validators_accounts_hash_mismatch = true;
    }

    if matches.value_of("signer_addr").is_some() {
        warn!("--vote-signer-address ignored");
    }

    let entrypoint_addr = matches.value_of("entrypoint").map(|entrypoint| {
        solana_net_utils::parse_host_port(entrypoint).unwrap_or_else(|e| {
            eprintln!("failed to parse entrypoint address: {}", e);
            exit(1);
        })
    });

    let public_rpc_addr = matches.value_of("public_rpc_addr").map(|addr| {
        solana_net_utils::parse_host_port(addr).unwrap_or_else(|e| {
            eprintln!("failed to parse public rpc address: {}", e);
            exit(1);
        })
    });

    let logfile = {
        let logfile = matches
            .value_of("logfile")
            .map(|s| s.into())
            .unwrap_or_else(|| format!("solana-validator-{}.log", identity_keypair.pubkey()));

        if logfile == "-" {
            None
        } else {
            println!("log file: {}", logfile);
            Some(logfile)
        }
    };
    let _logger_thread = start_logger(logfile);

    // Default to RUST_BACKTRACE=1 for more informative validator logs
    if env::var_os("RUST_BACKTRACE").is_none() {
        env::set_var("RUST_BACKTRACE", "1")
    }

    let gossip_host = if let Some(entrypoint_addr) = entrypoint_addr {
        solana_net_utils::get_public_ip_addr(&entrypoint_addr).unwrap_or_else(|err| {
            eprintln!(
                "Failed to contact cluster entrypoint {}: {}",
                entrypoint_addr, err
            );
            exit(1);
        })
    } else {
        solana_net_utils::parse_host(matches.value_of("gossip_host").unwrap_or("127.0.0.1"))
            .unwrap_or_else(|err| {
                eprintln!("Error: {}", err);
                exit(1);
            })
    };

    let gossip_addr = SocketAddr::new(
        gossip_host,
        value_t!(matches, "gossip_port", u16).unwrap_or_else(|_| {
            solana_net_utils::find_available_port_in_range(bind_address, (0, 1)).unwrap_or_else(
                |err| {
                    eprintln!("Unable to find an available gossip port: {}", err);
                    exit(1);
                },
            )
        }),
    );

    let cluster_entrypoint = entrypoint_addr
        .as_ref()
        .map(ContactInfo::new_gossip_entry_point);

    let mut node = Node::new_with_external_ip(
        &identity_keypair.pubkey(),
        &gossip_addr,
        dynamic_port_range,
        bind_address,
    );

    if restricted_repair_only_mode {
        let any = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)), 0);
        // When in --restricted_repair_only_mode is enabled only the gossip and repair ports
        // need to be reachable by the entrypoint to respond to gossip pull requests and repair
        // requests initiated by the node.  All other ports are unused.
        node.info.tpu = any;
        node.info.tpu_forwards = any;
        node.info.tvu = any;
        node.info.tvu_forwards = any;
        node.info.serve_repair = any;

        // A node in this configuration shouldn't be an entrypoint to other nodes
        node.sockets.ip_echo = None;
    }

    if !private_rpc {
        if let Some(public_rpc_addr) = public_rpc_addr {
            node.info.rpc = public_rpc_addr;
            node.info.rpc_pubsub = public_rpc_addr;
            node.info.rpc_banks = public_rpc_addr;
        } else if let Some((rpc_addr, rpc_pubsub_addr, rpc_banks_addr)) = validator_config.rpc_addrs
        {
            node.info.rpc = SocketAddr::new(node.info.gossip.ip(), rpc_addr.port());
            node.info.rpc_pubsub = SocketAddr::new(node.info.gossip.ip(), rpc_pubsub_addr.port());
            node.info.rpc_banks = SocketAddr::new(node.info.gossip.ip(), rpc_banks_addr.port());
        }
    }

    info!("{} {}", crate_name!(), solana_version::version!());
    info!("Starting validator with: {:#?}", std::env::args_os());

    solana_metrics::set_host_id(identity_keypair.pubkey().to_string());
    solana_metrics::set_panic_hook("validator");

    let validator = create_validator(
        node,
        &identity_keypair,
        &ledger_path,
        &vote_account,
        authorized_voter_keypairs,
        cluster_entrypoint,
        validator_config,
        rpc_bootstrap_config,
        no_port_check,
    );

    if let Some(filename) = init_complete_file {
        File::create(filename).unwrap_or_else(|_| {
            error!("Unable to create: {}", filename);
            exit(1);
        });
    }
    info!("Validator initialized");
    validator.join().expect("validator exit");
    info!("Validator exiting..");
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_interval_check() {
        assert!(!is_snapshot_config_invalid(0, 100));
        assert!(is_snapshot_config_invalid(1, 100));
        assert!(is_snapshot_config_invalid(230, 100));
        assert!(!is_snapshot_config_invalid(500, 100));
        assert!(!is_snapshot_config_invalid(5, 5));
    }
}
