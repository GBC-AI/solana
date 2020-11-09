use crate::{
    cli::{CliCommand, CliCommandInfo, CliConfig, CliError, ProcessResult},
    spend_utils::{resolve_spend_tx_and_check_account_balance, SpendAmount},
};
use chrono::{Local, TimeZone};
use clap::{value_t, value_t_or_exit, App, AppSettings, Arg, ArgMatches, SubCommand};
use console::{style, Emoji};
use solana_clap_utils::{
    commitment::commitment_arg, input_parsers::*, input_validators::*, keypair::DefaultSigner,
};
use solana_cli_output::{
    display::{
        format_labeled_address, new_spinner_progress_bar, println_name_value, println_transaction,
    },
    *,
};
use solana_client::{
    client_error::ClientErrorKind,
    pubsub_client::PubsubClient,
    rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClient},
    rpc_config::{
        RpcAccountInfoConfig, RpcLargestAccountsConfig, RpcLargestAccountsFilter,
        RpcProgramAccountsConfig,
    },
    rpc_filter,
    rpc_response::SlotInfo,
};
use solana_remote_wallet::remote_wallet::RemoteWalletManager;
use solana_sdk::{
    account::from_account,
    account_utils::StateMut,
    clock::{self, Clock, Slot},
    commitment_config::CommitmentConfig,
    epoch_schedule::Epoch,
    message::Message,
    native_token::lamports_to_sol,
    pubkey::{self, Pubkey},
    signature::Signature,
    system_instruction, system_program,
    sysvar::{
        self,
        stake_history::{self},
    },
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::sleep,
    time::{Duration, Instant},
};

static CHECK_MARK: Emoji = Emoji("✅ ", "");
static CROSS_MARK: Emoji = Emoji("❌ ", "");

pub trait ClusterQuerySubCommands {
    fn cluster_query_subcommands(self) -> Self;
}

impl ClusterQuerySubCommands for App<'_, '_> {
    fn cluster_query_subcommands(self) -> Self {
        self.subcommand(
            SubCommand::with_name("block")
                .about("Get a confirmed block")
                .arg(
                    Arg::with_name("slot")
                        .long("slot")
                        .validator(is_slot)
                        .value_name("SLOT")
                        .takes_value(true)
                        .index(1),
                ),
        )
        .subcommand(
            SubCommand::with_name("catchup")
                .about("Wait for a validator to catch up to the cluster")
                .arg(
                    pubkey!(Arg::with_name("node_pubkey")
                        .index(1)
                        .value_name("VALIDATOR_PUBKEY")
                        .required(true),
                        "Identity pubkey of the validator"),
                )
                .arg(
                    Arg::with_name("node_json_rpc_url")
                        .index(2)
                        .value_name("URL")
                        .takes_value(true)
                        .validator(is_url)
                        .help("JSON RPC URL for validator, which is useful for validators with a private RPC service")
                )
                .arg(
                    Arg::with_name("follow")
                        .long("follow")
                        .takes_value(false)
                        .help("Continue reporting progress even after the validator has caught up"),
                )
                .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("cluster-date")
                .about("Get current cluster date, computed from genesis creation time and network time")
        )
        .subcommand(
            SubCommand::with_name("cluster-version")
                .about("Get the version of the cluster entrypoint"),
        )
        .subcommand(SubCommand::with_name("fees").about("Display current cluster fees"))
        .subcommand(
            SubCommand::with_name("first-available-block")
                .about("Get the first available block in the storage"),
        )
        .subcommand(SubCommand::with_name("block-time")
            .about("Get estimated production time of a block")
            .alias("get-block-time")
            .arg(
                Arg::with_name("slot")
                    .index(1)
                    .takes_value(true)
                    .value_name("SLOT")
                    .help("Slot number of the block to query")
            )
        )
        .subcommand(SubCommand::with_name("leader-schedule").about("Display leader schedule"))
        .subcommand(
            SubCommand::with_name("epoch-info")
            .about("Get information about the current epoch")
            .alias("get-epoch-info")
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("genesis-hash")
            .about("Get the genesis hash")
            .alias("get-genesis-hash")
        )
        .subcommand(
            SubCommand::with_name("slot").about("Get current slot")
            .alias("get-slot")
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("block-height").about("Get current block height")
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("epoch").about("Get current epoch")
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("largest-accounts").about("Get addresses of largest cluster accounts")
            .arg(
                Arg::with_name("circulating")
                    .long("circulating")
                    .takes_value(false)
                    .help("Filter address list to only circulating accounts")
            )
            .arg(
                Arg::with_name("non_circulating")
                    .long("non-circulating")
                    .takes_value(false)
                    .conflicts_with("circulating")
                    .help("Filter address list to only non-circulating accounts")
            )
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("supply").about("Get information about the cluster supply of SOL")
            .arg(
                Arg::with_name("print_accounts")
                    .long("print-accounts")
                    .takes_value(false)
                    .help("Print list of non-circualting account addresses")
            )
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("total-supply").about("Get total number of SOL")
            .setting(AppSettings::Hidden)
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("transaction-count").about("Get current transaction count")
            .alias("get-transaction-count")
            .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("ping")
                .about("Submit transactions sequentially")
                .arg(
                    Arg::with_name("interval")
                        .short("i")
                        .long("interval")
                        .value_name("SECONDS")
                        .takes_value(true)
                        .default_value("2")
                        .help("Wait interval seconds between submitting the next transaction"),
                )
                .arg(
                    Arg::with_name("count")
                        .short("c")
                        .long("count")
                        .value_name("NUMBER")
                        .takes_value(true)
                        .help("Stop after submitting count transactions"),
                )
                .arg(
                    Arg::with_name("lamports")
                        .long("lamports")
                        .value_name("NUMBER")
                        .takes_value(true)
                        .default_value("1")
                        .validator(is_amount)
                        .help("Number of lamports to transfer for each transaction"),
                )
                .arg(
                    Arg::with_name("timeout")
                        .short("t")
                        .long("timeout")
                        .value_name("SECONDS")
                        .takes_value(true)
                        .default_value("15")
                        .help("Wait up to timeout seconds for transaction confirmation"),
                )
                .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("live-slots")
                .about("Show information about the current slot progression"),
        )
        .subcommand(
            SubCommand::with_name("block-production")
                .about("Show information about block production")
                .alias("show-block-production")
                .arg(
                    Arg::with_name("epoch")
                        .long("epoch")
                        .takes_value(true)
                        .help("Epoch to show block production for [default: current epoch]"),
                )
                .arg(
                    Arg::with_name("slot_limit")
                        .long("slot-limit")
                        .takes_value(true)
                        .help("Limit results to this many slots from the end of the epoch [default: full epoch]"),
                ),
        )
        .subcommand(
            SubCommand::with_name("gossip")
                .about("Show the current gossip network nodes")
                .alias("show-gossip")
        )
        .subcommand(
            SubCommand::with_name("stakes")
                .about("Show stake account information")
                .arg(
                    pubkey!(Arg::with_name("vote_account_pubkeys")
                        .index(1)
                        .value_name("VOTE_ACCOUNT_PUBKEYS")
                        .multiple(true),
                        "Only show stake accounts delegated to the provided vote accounts. "),
                )
                .arg(
                    Arg::with_name("lamports")
                        .long("lamports")
                        .takes_value(false)
                        .help("Display balance in lamports instead of SOL"),
                ),
        )
        .subcommand(
            SubCommand::with_name("validators")
                .about("Show summary information about the current validators")
                .alias("show-validators")
                .arg(
                    Arg::with_name("lamports")
                        .long("lamports")
                        .takes_value(false)
                        .help("Display balance in lamports instead of SOL"),
                )
                .arg(commitment_arg()),
        )
        .subcommand(
            SubCommand::with_name("transaction-history")
                .about("Show historical transactions affecting the given address \
                        from newest to oldest")
                .arg(
                    pubkey!(Arg::with_name("address")
                        .index(1)
                        .value_name("ADDRESS")
                        .required(true),
                        "Account address"),
                )
                .arg(
                    Arg::with_name("limit")
                        .long("limit")
                        .takes_value(true)
                        .value_name("LIMIT")
                        .validator(is_slot)
                        .default_value("1000")
                        .help("Maximum number of transaction signatures to return"),
                )
                .arg(
                    Arg::with_name("before")
                        .long("before")
                        .value_name("TRANSACTION_SIGNATURE")
                        .takes_value(true)
                        .help("Start with the first signature older than this one"),
                )
                .arg(
                    Arg::with_name("show_transactions")
                        .long("show-transactions")
                        .takes_value(false)
                        .help("Display the full transactions"),
                )
        )
    }
}

pub fn parse_catchup(
    matches: &ArgMatches<'_>,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let node_pubkey = pubkey_of_signer(matches, "node_pubkey", wallet_manager)?.unwrap();
    let node_json_rpc_url = value_t!(matches, "node_json_rpc_url", String).ok();
    let follow = matches.is_present("follow");
    Ok(CliCommandInfo {
        command: CliCommand::Catchup {
            node_pubkey,
            node_json_rpc_url,
            follow,
        },
        signers: vec![],
    })
}

pub fn parse_cluster_ping(
    matches: &ArgMatches<'_>,
    default_signer: &DefaultSigner,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let lamports = value_t_or_exit!(matches, "lamports", u64);
    let interval = Duration::from_secs(value_t_or_exit!(matches, "interval", u64));
    let count = if matches.is_present("count") {
        Some(value_t_or_exit!(matches, "count", u64))
    } else {
        None
    };
    let timeout = Duration::from_secs(value_t_or_exit!(matches, "timeout", u64));
    Ok(CliCommandInfo {
        command: CliCommand::Ping {
            lamports,
            interval,
            count,
            timeout,
        },
        signers: vec![default_signer.signer_from_path(matches, wallet_manager)?],
    })
}

pub fn parse_get_block(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    let slot = value_of(matches, "slot");
    Ok(CliCommandInfo {
        command: CliCommand::GetBlock { slot },
        signers: vec![],
    })
}

pub fn parse_get_block_time(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    let slot = value_of(matches, "slot");
    Ok(CliCommandInfo {
        command: CliCommand::GetBlockTime { slot },
        signers: vec![],
    })
}

pub fn parse_get_epoch(_matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    Ok(CliCommandInfo {
        command: CliCommand::GetEpoch,
        signers: vec![],
    })
}

pub fn parse_get_epoch_info(_matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    Ok(CliCommandInfo {
        command: CliCommand::GetEpochInfo,
        signers: vec![],
    })
}

pub fn parse_get_slot(_matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    Ok(CliCommandInfo {
        command: CliCommand::GetSlot,
        signers: vec![],
    })
}

pub fn parse_get_block_height(_matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    Ok(CliCommandInfo {
        command: CliCommand::GetBlockHeight,
        signers: vec![],
    })
}

pub fn parse_largest_accounts(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    let filter = if matches.is_present("circulating") {
        Some(RpcLargestAccountsFilter::Circulating)
    } else if matches.is_present("non_circulating") {
        Some(RpcLargestAccountsFilter::NonCirculating)
    } else {
        None
    };
    Ok(CliCommandInfo {
        command: CliCommand::LargestAccounts { filter },
        signers: vec![],
    })
}

pub fn parse_supply(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    let print_accounts = matches.is_present("print_accounts");
    Ok(CliCommandInfo {
        command: CliCommand::Supply { print_accounts },
        signers: vec![],
    })
}

pub fn parse_total_supply(_matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    Ok(CliCommandInfo {
        command: CliCommand::TotalSupply,
        signers: vec![],
    })
}

pub fn parse_get_transaction_count(_matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    Ok(CliCommandInfo {
        command: CliCommand::GetTransactionCount,
        signers: vec![],
    })
}

pub fn parse_show_stakes(
    matches: &ArgMatches<'_>,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let use_lamports_unit = matches.is_present("lamports");
    let vote_account_pubkeys =
        pubkeys_of_multiple_signers(matches, "vote_account_pubkeys", wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::ShowStakes {
            use_lamports_unit,
            vote_account_pubkeys,
        },
        signers: vec![],
    })
}

pub fn parse_show_validators(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    let use_lamports_unit = matches.is_present("lamports");

    Ok(CliCommandInfo {
        command: CliCommand::ShowValidators { use_lamports_unit },
        signers: vec![],
    })
}

pub fn parse_transaction_history(
    matches: &ArgMatches<'_>,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let address = pubkey_of_signer(matches, "address", wallet_manager)?.unwrap();

    let before = match matches.value_of("before") {
        Some(signature) => Some(
            signature
                .parse()
                .map_err(|err| CliError::BadParameter(format!("Invalid signature: {}", err)))?,
        ),
        None => None,
    };
    let until = match matches.value_of("until") {
        Some(signature) => Some(
            signature
                .parse()
                .map_err(|err| CliError::BadParameter(format!("Invalid signature: {}", err)))?,
        ),
        None => None,
    };
    let limit = value_t_or_exit!(matches, "limit", usize);
    let show_transactions = matches.is_present("show_transactions");

    Ok(CliCommandInfo {
        command: CliCommand::TransactionHistory {
            address,
            before,
            until,
            limit,
            show_transactions,
        },
        signers: vec![],
    })
}

pub fn process_catchup(
    rpc_client: &RpcClient,
    config: &CliConfig,
    node_pubkey: &Pubkey,
    node_json_rpc_url: &Option<String>,
    follow: bool,
) -> ProcessResult {
    let sleep_interval = 5;

    let progress_bar = new_spinner_progress_bar();
    progress_bar.set_message("Connecting...");

    let node_client = if let Some(node_json_rpc_url) = node_json_rpc_url {
        RpcClient::new(node_json_rpc_url.to_string())
    } else {
        let rpc_addr = loop {
            let cluster_nodes = rpc_client.get_cluster_nodes()?;
            if let Some(contact_info) = cluster_nodes
                .iter()
                .find(|contact_info| contact_info.pubkey == node_pubkey.to_string())
            {
                if let Some(rpc_addr) = contact_info.rpc {
                    break rpc_addr;
                }
                progress_bar.set_message(&format!("RPC service not found for {}", node_pubkey));
            } else {
                progress_bar.set_message(&format!(
                    "Contact information not found for {}",
                    node_pubkey
                ));
            }
            sleep(Duration::from_secs(sleep_interval as u64));
        };

        RpcClient::new_socket(rpc_addr)
    };

    let reported_node_pubkey = loop {
        match node_client.get_identity() {
            Ok(reported_node_pubkey) => break reported_node_pubkey,
            Err(err) => {
                if let ClientErrorKind::Reqwest(err) = err.kind() {
                    progress_bar.set_message(&format!("Connection failed: {}", err));
                    sleep(Duration::from_secs(sleep_interval as u64));
                    continue;
                }
                return Err(Box::new(err));
            }
        }
    };

    if reported_node_pubkey != *node_pubkey {
        return Err(format!(
            "The identity reported by node RPC URL does not match.  Expected: {:?}.  Reported: {:?}",
            node_pubkey, reported_node_pubkey
        )
        .into());
    }

    if rpc_client.get_identity()? == *node_pubkey {
        return Err("Both RPC URLs reference the same node, unable to monitor for catchup.  Try a different --url".into());
    }

    let mut previous_rpc_slot = std::u64::MAX;
    let mut previous_slot_distance = 0;
    loop {
        let rpc_slot = rpc_client.get_slot_with_commitment(config.commitment)?;
        let node_slot = node_client.get_slot_with_commitment(config.commitment)?;
        if !follow && node_slot > std::cmp::min(previous_rpc_slot, rpc_slot) {
            progress_bar.finish_and_clear();
            return Ok(format!(
                "{} has caught up (us:{} them:{})",
                node_pubkey, node_slot, rpc_slot,
            ));
        }

        let slot_distance = rpc_slot as i64 - node_slot as i64;
        let slots_per_second =
            (previous_slot_distance - slot_distance) as f64 / f64::from(sleep_interval);
        let time_remaining = (slot_distance as f64 / slots_per_second).round();
        let time_remaining = if !time_remaining.is_normal() || time_remaining <= 0.0 {
            "".to_string()
        } else {
            format!(
                ". Time remaining: {}",
                humantime::format_duration(Duration::from_secs_f64(time_remaining))
            )
        };

        progress_bar.set_message(&format!(
            "{} slots behind (us:{} them:{}){}",
            slot_distance,
            node_slot,
            rpc_slot,
            if slot_distance == 0 || previous_rpc_slot == std::u64::MAX {
                "".to_string()
            } else {
                format!(
                    ", {} at {:.1} slots/second{}",
                    if slots_per_second < 0.0 {
                        "falling behind"
                    } else {
                        "gaining"
                    },
                    slots_per_second,
                    time_remaining
                )
            }
        ));

        sleep(Duration::from_secs(sleep_interval as u64));
        previous_rpc_slot = rpc_slot;
        previous_slot_distance = slot_distance;
    }
}

pub fn process_cluster_date(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let result = rpc_client
        .get_account_with_commitment(&sysvar::clock::id(), CommitmentConfig::default())?;
    if let Some(clock_account) = result.value {
        let clock: Clock = from_account(&clock_account).ok_or_else(|| {
            CliError::RpcRequestError("Failed to deserialize clock sysvar".to_string())
        })?;
        let block_time = CliBlockTime {
            slot: result.context.slot,
            timestamp: clock.unix_timestamp,
        };
        Ok(config.output_format.formatted_string(&block_time))
    } else {
        Err(format!("AccountNotFound: pubkey={}", sysvar::clock::id()).into())
    }
}

pub fn process_cluster_version(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let remote_version = rpc_client.get_version()?;

    if config.verbose {
        Ok(format!("{:?}", remote_version))
    } else {
        Ok(remote_version.to_string())
    }
}

pub fn process_fees(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let result = rpc_client.get_recent_blockhash_with_commitment(CommitmentConfig::default())?;
    let (recent_blockhash, fee_calculator, last_valid_slot) = result.value;
    let fees = CliFees {
        slot: result.context.slot,
        blockhash: recent_blockhash.to_string(),
        lamports_per_signature: fee_calculator.lamports_per_signature,
        last_valid_slot,
    };
    Ok(config.output_format.formatted_string(&fees))
}

pub fn process_first_available_block(rpc_client: &RpcClient) -> ProcessResult {
    let first_available_block = rpc_client.get_first_available_block()?;
    Ok(format!("{}", first_available_block))
}

pub fn process_leader_schedule(rpc_client: &RpcClient) -> ProcessResult {
    let epoch_info = rpc_client.get_epoch_info()?;
    let first_slot_in_epoch = epoch_info.absolute_slot - epoch_info.slot_index;

    let leader_schedule = rpc_client.get_leader_schedule(Some(first_slot_in_epoch))?;
    if leader_schedule.is_none() {
        return Err(format!(
            "Unable to fetch leader schedule for slot {}",
            first_slot_in_epoch
        )
        .into());
    }
    let leader_schedule = leader_schedule.unwrap();

    let mut leader_per_slot_index = Vec::new();
    for (pubkey, leader_slots) in leader_schedule.iter() {
        for slot_index in leader_slots.iter() {
            if *slot_index >= leader_per_slot_index.len() {
                leader_per_slot_index.resize(*slot_index + 1, "?");
            }
            leader_per_slot_index[*slot_index] = pubkey;
        }
    }

    for (slot_index, leader) in leader_per_slot_index.iter().enumerate() {
        println!(
            "  {:<15} {:<44}",
            first_slot_in_epoch + slot_index as u64,
            leader
        );
    }

    Ok("".to_string())
}

pub fn process_get_block(
    rpc_client: &RpcClient,
    _config: &CliConfig,
    slot: Option<Slot>,
) -> ProcessResult {
    let slot = if let Some(slot) = slot {
        slot
    } else {
        rpc_client.get_slot()?
    };

    let mut block =
        rpc_client.get_confirmed_block_with_encoding(slot, UiTransactionEncoding::Base64)?;

    println!("Slot: {}", slot);
    println!("Parent Slot: {}", block.parent_slot);
    println!("Blockhash: {}", block.blockhash);
    println!("Previous Blockhash: {}", block.previous_blockhash);
    if let Some(block_time) = block.block_time {
        println!("Block Time: {:?}", Local.timestamp(block_time, 0));
    }
    if !block.rewards.is_empty() {
        block.rewards.sort_by(|a, b| a.pubkey.cmp(&b.pubkey));
        let mut total_rewards = 0;
        println!("Rewards:",);
        println!(
            "  {:<44}  {:^15}  {:<15}  {:<20}  {:>14}",
            "Address", "Type", "Amount", "New Balance", "Percent Change"
        );
        for reward in block.rewards {
            let sign = if reward.lamports < 0 { "-" } else { "" };

            total_rewards += reward.lamports;
            println!(
                "  {:<44}  {:^15}  {:>15}  {}",
                reward.pubkey,
                if let Some(reward_type) = reward.reward_type {
                    format!("{}", reward_type)
                } else {
                    "-".to_string()
                },
                format!(
                    "{}◎{:<14.9}",
                    sign,
                    lamports_to_sol(reward.lamports.abs() as u64)
                ),
                if reward.post_balance == 0 {
                    "          -                 -".to_string()
                } else {
                    format!(
                        "◎{:<19.9}  {:>13.9}%",
                        lamports_to_sol(reward.post_balance),
                        reward.lamports.abs() as f64
                            / (reward.post_balance as f64 - reward.lamports as f64)
                    )
                }
            );
        }

        let sign = if total_rewards < 0 { "-" } else { "" };
        println!(
            "Total Rewards: {}◎{:<12.9}",
            sign,
            lamports_to_sol(total_rewards.abs() as u64)
        );
    }
    for (index, transaction_with_meta) in block.transactions.iter().enumerate() {
        println!("Transaction {}:", index);
        println_transaction(
            &transaction_with_meta.transaction.decode().unwrap(),
            &transaction_with_meta.meta,
            "  ",
        );
    }
    Ok("".to_string())
}

pub fn process_get_block_time(
    rpc_client: &RpcClient,
    config: &CliConfig,
    slot: Option<Slot>,
) -> ProcessResult {
    let slot = if let Some(slot) = slot {
        slot
    } else {
        rpc_client.get_slot()?
    };
    let timestamp = rpc_client.get_block_time(slot)?;
    let block_time = CliBlockTime { slot, timestamp };
    Ok(config.output_format.formatted_string(&block_time))
}

pub fn process_get_epoch(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let epoch_info = rpc_client.get_epoch_info_with_commitment(config.commitment)?;
    Ok(epoch_info.epoch.to_string())
}

pub fn process_get_epoch_info(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let epoch_info: CliEpochInfo = rpc_client
        .get_epoch_info_with_commitment(config.commitment)?
        .into();
    Ok(config.output_format.formatted_string(&epoch_info))
}

pub fn process_get_genesis_hash(rpc_client: &RpcClient) -> ProcessResult {
    let genesis_hash = rpc_client.get_genesis_hash()?;
    Ok(genesis_hash.to_string())
}

pub fn process_get_slot(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let slot = rpc_client.get_slot_with_commitment(config.commitment)?;
    Ok(slot.to_string())
}

pub fn process_get_block_height(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let epoch_info: CliEpochInfo = rpc_client
        .get_epoch_info_with_commitment(config.commitment)?
        .into();
    Ok(epoch_info.epoch_info.block_height.to_string())
}

pub fn parse_show_block_production(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    let epoch = value_t!(matches, "epoch", Epoch).ok();
    let slot_limit = value_t!(matches, "slot_limit", u64).ok();

    Ok(CliCommandInfo {
        command: CliCommand::ShowBlockProduction { epoch, slot_limit },
        signers: vec![],
    })
}

pub fn process_show_block_production(
    rpc_client: &RpcClient,
    config: &CliConfig,
    epoch: Option<Epoch>,
    slot_limit: Option<u64>,
) -> ProcessResult {
    let epoch_schedule = rpc_client.get_epoch_schedule()?;
    let epoch_info = rpc_client.get_epoch_info_with_commitment(CommitmentConfig::root())?;

    let epoch = epoch.unwrap_or(epoch_info.epoch);
    if epoch > epoch_info.epoch {
        return Err(format!("Epoch {} is in the future", epoch).into());
    }

    let minimum_ledger_slot = rpc_client.minimum_ledger_slot()?;

    let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(epoch);
    let end_slot = std::cmp::min(
        epoch_info.absolute_slot,
        epoch_schedule.get_last_slot_in_epoch(epoch),
    );

    let mut start_slot = if let Some(slot_limit) = slot_limit {
        std::cmp::max(end_slot.saturating_sub(slot_limit), first_slot_in_epoch)
    } else {
        first_slot_in_epoch
    };

    if minimum_ledger_slot > end_slot {
        return Err(format!(
            "Ledger data not available for slots {} to {} (minimum ledger slot is {})",
            start_slot, end_slot, minimum_ledger_slot
        )
        .into());
    }

    if minimum_ledger_slot > start_slot {
        println!(
            "\n{}",
            style(format!(
                "Note: Requested start slot was {} but minimum ledger slot is {}",
                start_slot, minimum_ledger_slot
            ))
            .italic(),
        );
        start_slot = minimum_ledger_slot;
    }

    let progress_bar = new_spinner_progress_bar();
    progress_bar.set_message(&format!(
        "Fetching confirmed blocks between slots {} and {}...",
        start_slot, end_slot
    ));
    let confirmed_blocks = rpc_client.get_confirmed_blocks(start_slot, Some(end_slot))?;

    let start_slot_index = (start_slot - first_slot_in_epoch) as usize;
    let end_slot_index = (end_slot - first_slot_in_epoch) as usize;
    let total_slots = end_slot_index - start_slot_index + 1;
    let total_blocks_produced = confirmed_blocks.len();
    assert!(total_blocks_produced <= total_slots);
    let total_slots_skipped = total_slots - total_blocks_produced;
    let mut leader_slot_count = HashMap::new();
    let mut leader_skipped_slots = HashMap::new();

    progress_bar.set_message(&format!("Fetching leader schedule for epoch {}...", epoch));
    let leader_schedule = rpc_client
        .get_leader_schedule_with_commitment(Some(start_slot), CommitmentConfig::root())?;
    if leader_schedule.is_none() {
        return Err(format!("Unable to fetch leader schedule for slot {}", start_slot).into());
    }
    let leader_schedule = leader_schedule.unwrap();

    let mut leader_per_slot_index = Vec::new();
    leader_per_slot_index.resize(total_slots, "?".to_string());
    for (pubkey, leader_slots) in leader_schedule.iter() {
        let pubkey = format_labeled_address(pubkey, &config.address_labels);
        for slot_index in leader_slots.iter() {
            if *slot_index >= start_slot_index && *slot_index <= end_slot_index {
                leader_per_slot_index[*slot_index - start_slot_index] = pubkey.clone();
            }
        }
    }

    progress_bar.set_message(&format!(
        "Processing {} slots containing {} blocks and {} empty slots...",
        total_slots, total_blocks_produced, total_slots_skipped
    ));

    let mut confirmed_blocks_index = 0;
    let mut individual_slot_status = vec![];
    for (slot_index, leader) in leader_per_slot_index.iter().enumerate() {
        let slot = start_slot + slot_index as u64;
        let slot_count = leader_slot_count.entry(leader).or_insert(0);
        *slot_count += 1;
        let skipped_slots = leader_skipped_slots.entry(leader).or_insert(0);

        loop {
            if confirmed_blocks_index < confirmed_blocks.len() {
                let slot_of_next_confirmed_block = confirmed_blocks[confirmed_blocks_index];
                if slot_of_next_confirmed_block < slot {
                    confirmed_blocks_index += 1;
                    continue;
                }
                if slot_of_next_confirmed_block == slot {
                    individual_slot_status.push(CliSlotStatus {
                        slot,
                        leader: (*leader).to_string(),
                        skipped: false,
                    });
                    break;
                }
            }
            *skipped_slots += 1;
            individual_slot_status.push(CliSlotStatus {
                slot,
                leader: (*leader).to_string(),
                skipped: true,
            });
            break;
        }
    }

    progress_bar.finish_and_clear();

    let mut leaders: Vec<CliBlockProductionEntry> = leader_slot_count
        .iter()
        .map(|(leader, leader_slots)| {
            let skipped_slots = leader_skipped_slots.get(leader).unwrap();
            let blocks_produced = leader_slots - skipped_slots;
            CliBlockProductionEntry {
                identity_pubkey: (**leader).to_string(),
                leader_slots: *leader_slots,
                blocks_produced,
                skipped_slots: *skipped_slots,
            }
        })
        .collect();
    leaders.sort_by(|a, b| a.identity_pubkey.partial_cmp(&b.identity_pubkey).unwrap());
    let block_production = CliBlockProduction {
        epoch,
        start_slot,
        end_slot,
        total_slots,
        total_blocks_produced,
        total_slots_skipped,
        leaders,
        individual_slot_status,
        verbose: config.verbose,
    };
    Ok(config.output_format.formatted_string(&block_production))
}

pub fn process_largest_accounts(
    rpc_client: &RpcClient,
    config: &CliConfig,
    filter: Option<RpcLargestAccountsFilter>,
) -> ProcessResult {
    let accounts = rpc_client
        .get_largest_accounts_with_config(RpcLargestAccountsConfig {
            commitment: Some(config.commitment),
            filter,
        })?
        .value;
    let largest_accounts = CliAccountBalances { accounts };
    Ok(config.output_format.formatted_string(&largest_accounts))
}

pub fn process_supply(
    rpc_client: &RpcClient,
    config: &CliConfig,
    print_accounts: bool,
) -> ProcessResult {
    let supply_response = rpc_client.supply_with_commitment(config.commitment)?;
    let mut supply: CliSupply = supply_response.value.into();
    supply.print_accounts = print_accounts;
    Ok(config.output_format.formatted_string(&supply))
}

pub fn process_total_supply(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let total_supply = rpc_client.total_supply_with_commitment(config.commitment)?;
    Ok(format!("{} SOL", lamports_to_sol(total_supply)))
}

pub fn process_get_transaction_count(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let transaction_count = rpc_client.get_transaction_count_with_commitment(config.commitment)?;
    Ok(transaction_count.to_string())
}

pub fn process_ping(
    rpc_client: &RpcClient,
    config: &CliConfig,
    lamports: u64,
    interval: &Duration,
    count: &Option<u64>,
    timeout: &Duration,
) -> ProcessResult {
    println_name_value("Source Account:", &config.signers[0].pubkey().to_string());
    println!();

    let (signal_sender, signal_receiver) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = signal_sender.send(());
    })
    .expect("Error setting Ctrl-C handler");

    let mut submit_count = 0;
    let mut confirmed_count = 0;
    let mut confirmation_time: VecDeque<u64> = VecDeque::with_capacity(1024);

    let (mut blockhash, mut fee_calculator) = rpc_client.get_recent_blockhash()?;
    let mut blockhash_transaction_count = 0;
    let mut blockhash_acquired = Instant::now();
    'mainloop: for seq in 0..count.unwrap_or(std::u64::MAX) {
        let now = Instant::now();
        if now.duration_since(blockhash_acquired).as_secs() > 60 {
            // Fetch a new blockhash every minute
            let (new_blockhash, new_fee_calculator) = rpc_client.get_new_blockhash(&blockhash)?;
            blockhash = new_blockhash;
            fee_calculator = new_fee_calculator;
            blockhash_transaction_count = 0;
            blockhash_acquired = Instant::now();
        }

        let seed =
            &format!("{}{}", blockhash_transaction_count, blockhash)[0..pubkey::MAX_SEED_LEN];
        let to = Pubkey::create_with_seed(&config.signers[0].pubkey(), seed, &system_program::id())
            .unwrap();
        blockhash_transaction_count += 1;

        let build_message = |lamports| {
            let ix = system_instruction::transfer(&config.signers[0].pubkey(), &to, lamports);
            Message::new(&[ix], Some(&config.signers[0].pubkey()))
        };
        let (message, _) = resolve_spend_tx_and_check_account_balance(
            rpc_client,
            false,
            SpendAmount::Some(lamports),
            &fee_calculator,
            &config.signers[0].pubkey(),
            build_message,
            config.commitment,
        )?;
        let mut tx = Transaction::new_unsigned(message);
        tx.try_sign(&config.signers, blockhash)?;

        match rpc_client.send_transaction(&tx) {
            Ok(signature) => {
                let transaction_sent = Instant::now();
                loop {
                    let signature_status = rpc_client
                        .get_signature_status_with_commitment(&signature, config.commitment)?;
                    let elapsed_time = Instant::now().duration_since(transaction_sent);
                    if let Some(transaction_status) = signature_status {
                        match transaction_status {
                            Ok(()) => {
                                let elapsed_time_millis = elapsed_time.as_millis() as u64;
                                confirmation_time.push_back(elapsed_time_millis);
                                println!(
                                    "{}{} lamport(s) transferred: seq={:<3} time={:>4}ms signature={}",
                                    CHECK_MARK, lamports, seq, elapsed_time_millis, signature
                                );
                                confirmed_count += 1;
                            }
                            Err(err) => {
                                println!(
                                    "{}Transaction failed:    seq={:<3} error={:?} signature={}",
                                    CROSS_MARK, seq, err, signature
                                );
                            }
                        }
                        break;
                    }

                    if elapsed_time >= *timeout {
                        println!(
                            "{}Confirmation timeout:  seq={:<3}             signature={}",
                            CROSS_MARK, seq, signature
                        );
                        break;
                    }

                    // Sleep for half a slot
                    if signal_receiver
                        .recv_timeout(Duration::from_millis(
                            500 * clock::CFG.DEFAULT_TICKS_PER_SLOT
                                / clock::CFG.DEFAULT_TICKS_PER_SECOND,
                        ))
                        .is_ok()
                    {
                        break 'mainloop;
                    }
                }
            }
            Err(err) => {
                println!(
                    "{}Submit failed:         seq={:<3} error={:?}",
                    CROSS_MARK, seq, err
                );
            }
        }
        submit_count += 1;

        if signal_receiver.recv_timeout(*interval).is_ok() {
            break 'mainloop;
        }
    }

    println!();
    println!("--- transaction statistics ---");
    println!(
        "{} transactions submitted, {} transactions confirmed, {:.1}% transaction loss",
        submit_count,
        confirmed_count,
        (100. - f64::from(confirmed_count) / f64::from(submit_count) * 100.)
    );
    if !confirmation_time.is_empty() {
        let samples: Vec<f64> = confirmation_time.iter().map(|t| *t as f64).collect();
        let dist = criterion_stats::Distribution::from(samples.into_boxed_slice());
        let mean = dist.mean();
        println!(
            "confirmation min/mean/max/stddev = {:.0}/{:.0}/{:.0}/{:.0} ms",
            dist.min(),
            mean,
            dist.max(),
            dist.std_dev(Some(mean))
        );
    }

    Ok("".to_string())
}

pub fn process_live_slots(url: &str) -> ProcessResult {
    let exit = Arc::new(AtomicBool::new(false));

    // Disable Ctrl+C handler as sometimes the PubsubClient shutdown can stall.  Also it doesn't
    // really matter that the shutdown is clean because the process is terminating.
    /*
    let exit_clone = exit.clone();
    ctrlc::set_handler(move || {
        exit_clone.store(true, Ordering::Relaxed);
    })?;
    */

    let mut current: Option<SlotInfo> = None;
    let mut message = "".to_string();

    let slot_progress = new_spinner_progress_bar();
    slot_progress.set_message("Connecting...");
    let (mut client, receiver) = PubsubClient::slot_subscribe(url)?;
    slot_progress.set_message("Connected.");

    let spacer = "|";
    slot_progress.println(spacer);

    let mut last_root = std::u64::MAX;
    let mut last_root_update = Instant::now();
    let mut slots_per_second = std::f64::NAN;
    loop {
        if exit.load(Ordering::Relaxed) {
            eprintln!("{}", message);
            client.shutdown().unwrap();
            break;
        }

        match receiver.recv() {
            Ok(new_info) => {
                if last_root == std::u64::MAX {
                    last_root = new_info.root;
                    last_root_update = Instant::now();
                }
                if last_root_update.elapsed().as_secs() >= 5 {
                    let root = new_info.root;
                    slots_per_second =
                        (root - last_root) as f64 / last_root_update.elapsed().as_secs() as f64;
                    last_root_update = Instant::now();
                    last_root = root;
                }

                message = if slots_per_second.is_nan() {
                    format!("{:?}", new_info)
                } else {
                    format!(
                        "{:?} | root slot advancing at {:.2} slots/second",
                        new_info, slots_per_second
                    )
                }
                .to_owned();
                slot_progress.set_message(&message);

                if let Some(previous) = current {
                    let slot_delta: i64 = new_info.slot as i64 - previous.slot as i64;
                    let root_delta: i64 = new_info.root as i64 - previous.root as i64;

                    //
                    // if slot has advanced out of step with the root, we detect
                    // a mismatch and output the slot information
                    //
                    if slot_delta != root_delta {
                        let prev_root = format!(
                            "|<--- {} <- … <- {} <- {}   (prev)",
                            previous.root, previous.parent, previous.slot
                        );
                        slot_progress.println(&prev_root);

                        let new_root = format!(
                            "|  '- {} <- … <- {} <- {}   (next)",
                            new_info.root, new_info.parent, new_info.slot
                        );

                        slot_progress.println(prev_root);
                        slot_progress.println(new_root);
                        slot_progress.println(spacer);
                    }
                }
                current = Some(new_info);
            }
            Err(err) => {
                eprintln!("disconnected: {}", err);
                break;
            }
        }
    }

    Ok("".to_string())
}

pub fn process_show_gossip(rpc_client: &RpcClient, config: &CliConfig) -> ProcessResult {
    let cluster_nodes = rpc_client.get_cluster_nodes()?;

    fn format_port(addr: Option<SocketAddr>) -> String {
        addr.map(|addr| addr.port().to_string())
            .unwrap_or_else(|| "none".to_string())
    }

    let s: Vec<_> = cluster_nodes
        .into_iter()
        .map(|node| {
            format!(
                "{:15} | {:44} | {:6} | {:5} | {:21} | {}",
                node.gossip
                    .map(|addr| addr.ip().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                format_labeled_address(&node.pubkey, &config.address_labels),
                format_port(node.gossip),
                format_port(node.tpu),
                node.rpc
                    .map(|addr| addr.to_string())
                    .unwrap_or_else(|| "none".to_string()),
                node.version.unwrap_or_else(|| "unknown".to_string()),
            )
        })
        .collect();

    Ok(format!(
        "IP Address      | Node identifier                              \
         | Gossip | TPU   | RPC Address           | Version\n\
         ----------------+----------------------------------------------+\
         --------+-------+-----------------------+----------------\n\
         {}\n\
         Nodes: {}",
        s.join("\n"),
        s.len(),
    ))
}

pub fn process_show_stakes(
    rpc_client: &RpcClient,
    config: &CliConfig,
    use_lamports_unit: bool,
    vote_account_pubkeys: Option<&[Pubkey]>,
) -> ProcessResult {
    use crate::stake::build_stake_state;
    use solana_stake_program::stake_state::StakeState;

    let progress_bar = new_spinner_progress_bar();
    progress_bar.set_message("Fetching stake accounts...");

    let mut program_accounts_config = RpcProgramAccountsConfig {
        filters: None,
        account_config: RpcAccountInfoConfig {
            encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
            ..RpcAccountInfoConfig::default()
        },
    };

    if let Some(vote_account_pubkeys) = vote_account_pubkeys {
        // Use server-side filtering if only one vote account is provided
        if vote_account_pubkeys.len() == 1 {
            program_accounts_config.filters = Some(vec![
                // Filter by `StakeState::Stake(_, _)`
                rpc_filter::RpcFilterType::Memcmp(rpc_filter::Memcmp {
                    offset: 0,
                    bytes: rpc_filter::MemcmpEncodedBytes::Binary(
                        bs58::encode([2, 0, 0, 0]).into_string(),
                    ),
                    encoding: Some(rpc_filter::MemcmpEncoding::Binary),
                }),
                // Filter by `Delegation::voter_pubkey`, which begins at byte offset 124
                rpc_filter::RpcFilterType::Memcmp(rpc_filter::Memcmp {
                    offset: 124,
                    bytes: rpc_filter::MemcmpEncodedBytes::Binary(
                        vote_account_pubkeys[0].to_string(),
                    ),
                    encoding: Some(rpc_filter::MemcmpEncoding::Binary),
                }),
            ]);
        }
    }
    let all_stake_accounts = rpc_client
        .get_program_accounts_with_config(&solana_stake_program::id(), program_accounts_config)?;
    let stake_history_account = rpc_client.get_account(&stake_history::id())?;
    let clock_account = rpc_client.get_account(&sysvar::clock::id())?;
    let clock: Clock = from_account(&clock_account).ok_or_else(|| {
        CliError::RpcRequestError("Failed to deserialize clock sysvar".to_string())
    })?;
    progress_bar.finish_and_clear();

    let stake_history = from_account(&stake_history_account).ok_or_else(|| {
        CliError::RpcRequestError("Failed to deserialize stake history".to_string())
    })?;

    let mut stake_accounts: Vec<CliKeyedStakeState> = vec![];
    for (stake_pubkey, stake_account) in all_stake_accounts {
        if let Ok(stake_state) = stake_account.state() {
            match stake_state {
                StakeState::Initialized(_) => {
                    if vote_account_pubkeys.is_none() {
                        stake_accounts.push(CliKeyedStakeState {
                            stake_pubkey: stake_pubkey.to_string(),
                            stake_state: build_stake_state(
                                stake_account.lamports,
                                &stake_state,
                                use_lamports_unit,
                                &stake_history,
                                &clock,
                            ),
                        });
                    }
                }
                StakeState::Stake(_, stake) => {
                    if vote_account_pubkeys.is_none()
                        || vote_account_pubkeys
                            .unwrap()
                            .contains(&stake.delegation.voter_pubkey)
                    {
                        stake_accounts.push(CliKeyedStakeState {
                            stake_pubkey: stake_pubkey.to_string(),
                            stake_state: build_stake_state(
                                stake_account.lamports,
                                &stake_state,
                                use_lamports_unit,
                                &stake_history,
                                &clock,
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(config
        .output_format
        .formatted_string(&CliStakeVec::new(stake_accounts)))
}

pub fn process_show_validators(
    rpc_client: &RpcClient,
    config: &CliConfig,
    use_lamports_unit: bool,
) -> ProcessResult {
    let epoch_info = rpc_client.get_epoch_info_with_commitment(config.commitment)?;
    let vote_accounts = rpc_client.get_vote_accounts_with_commitment(config.commitment)?;

    let mut node_version = HashMap::new();
    let unknown_version = "unknown".to_string();
    for contact_info in rpc_client.get_cluster_nodes()? {
        node_version.insert(
            contact_info.pubkey,
            contact_info
                .version
                .unwrap_or_else(|| unknown_version.clone()),
        );
    }

    let total_active_stake = vote_accounts
        .current
        .iter()
        .chain(vote_accounts.delinquent.iter())
        .map(|vote_account| vote_account.activated_stake)
        .sum();

    let total_delinquent_stake = vote_accounts
        .delinquent
        .iter()
        .map(|vote_account| vote_account.activated_stake)
        .sum();
    let total_current_stake = total_active_stake - total_delinquent_stake;

    let mut current = vote_accounts.current;
    current.sort_by(|a, b| b.activated_stake.cmp(&a.activated_stake));
    let current_validators: Vec<CliValidator> = current
        .iter()
        .map(|vote_account| {
            CliValidator::new(
                vote_account,
                epoch_info.epoch,
                node_version
                    .get(&vote_account.node_pubkey)
                    .unwrap_or(&unknown_version)
                    .clone(),
                &config.address_labels,
            )
        })
        .collect();
    let mut delinquent = vote_accounts.delinquent;
    delinquent.sort_by(|a, b| b.activated_stake.cmp(&a.activated_stake));
    let delinquent_validators: Vec<CliValidator> = delinquent
        .iter()
        .map(|vote_account| {
            CliValidator::new(
                vote_account,
                epoch_info.epoch,
                node_version
                    .get(&vote_account.node_pubkey)
                    .unwrap_or(&unknown_version)
                    .clone(),
                &config.address_labels,
            )
        })
        .collect();

    let mut stake_by_version: BTreeMap<_, CliValidatorsStakeByVersion> = BTreeMap::new();
    for validator in current_validators.iter() {
        let mut entry = stake_by_version
            .entry(validator.version.clone())
            .or_default();
        entry.current_validators += 1;
        entry.current_active_stake += validator.activated_stake;
    }
    for validator in delinquent_validators.iter() {
        let mut entry = stake_by_version
            .entry(validator.version.clone())
            .or_default();
        entry.delinquent_validators += 1;
        entry.delinquent_active_stake += validator.activated_stake;
    }

    let cli_validators = CliValidators {
        total_active_stake,
        total_current_stake,
        total_delinquent_stake,
        current_validators,
        delinquent_validators,
        stake_by_version,
        use_lamports_unit,
    };
    Ok(config.output_format.formatted_string(&cli_validators))
}

pub fn process_transaction_history(
    rpc_client: &RpcClient,
    config: &CliConfig,
    address: &Pubkey,
    before: Option<Signature>,
    until: Option<Signature>,
    limit: usize,
    show_transactions: bool,
) -> ProcessResult {
    let results = rpc_client.get_confirmed_signatures_for_address2_with_config(
        address,
        GetConfirmedSignaturesForAddress2Config {
            before,
            until,
            limit: Some(limit),
        },
    )?;

    let transactions_found = format!("{} transactions found", results.len());

    for result in results {
        if config.verbose {
            println!(
                "{} [slot={} status={}] {}",
                result.signature,
                result.slot,
                match result.err {
                    None => "Confirmed".to_string(),
                    Some(err) => format!("Failed: {:?}", err),
                },
                result.memo.unwrap_or_else(|| "".to_string()),
            );
        } else {
            println!("{}", result.signature);
        }

        if show_transactions {
            if let Ok(signature) = result.signature.parse::<Signature>() {
                match rpc_client
                    .get_confirmed_transaction(&signature, UiTransactionEncoding::Base64)
                {
                    Ok(confirmed_transaction) => {
                        println_transaction(
                            &confirmed_transaction
                                .transaction
                                .transaction
                                .decode()
                                .expect("Successful decode"),
                            &confirmed_transaction.transaction.meta,
                            "  ",
                        );
                    }
                    Err(err) => println!("  Unable to get confirmed transaction details: {}", err),
                }
            }
            println!();
        }
    }
    Ok(transactions_found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{app, parse_command};
    use solana_sdk::signature::{write_keypair, Keypair};
    use tempfile::NamedTempFile;

    fn make_tmp_file() -> (String, NamedTempFile) {
        let tmp_file = NamedTempFile::new().unwrap();
        (String::from(tmp_file.path().to_str().unwrap()), tmp_file)
    }

    #[test]
    fn test_parse_command() {
        let test_commands = app("test", "desc", "version");
        let default_keypair = Keypair::new();
        let (default_keypair_file, mut tmp_file) = make_tmp_file();
        write_keypair(&default_keypair, tmp_file.as_file_mut()).unwrap();
        let default_signer = DefaultSigner {
            path: default_keypair_file,
            arg_name: String::new(),
        };

        let test_cluster_version = test_commands
            .clone()
            .get_matches_from(vec!["test", "cluster-date"]);
        assert_eq!(
            parse_command(&test_cluster_version, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ClusterDate,
                signers: vec![],
            }
        );

        let test_cluster_version = test_commands
            .clone()
            .get_matches_from(vec!["test", "cluster-version"]);
        assert_eq!(
            parse_command(&test_cluster_version, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::ClusterVersion,
                signers: vec![],
            }
        );

        let test_fees = test_commands.clone().get_matches_from(vec!["test", "fees"]);
        assert_eq!(
            parse_command(&test_fees, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::Fees,
                signers: vec![],
            }
        );

        let slot = 100;
        let test_get_block_time =
            test_commands
                .clone()
                .get_matches_from(vec!["test", "block-time", &slot.to_string()]);
        assert_eq!(
            parse_command(&test_get_block_time, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::GetBlockTime { slot: Some(slot) },
                signers: vec![],
            }
        );

        let test_get_epoch = test_commands
            .clone()
            .get_matches_from(vec!["test", "epoch"]);
        assert_eq!(
            parse_command(&test_get_epoch, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::GetEpoch,
                signers: vec![],
            }
        );

        let test_get_epoch_info = test_commands
            .clone()
            .get_matches_from(vec!["test", "epoch-info"]);
        assert_eq!(
            parse_command(&test_get_epoch_info, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::GetEpochInfo,
                signers: vec![],
            }
        );

        let test_get_genesis_hash = test_commands
            .clone()
            .get_matches_from(vec!["test", "genesis-hash"]);
        assert_eq!(
            parse_command(&test_get_genesis_hash, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::GetGenesisHash,
                signers: vec![],
            }
        );

        let test_get_slot = test_commands.clone().get_matches_from(vec!["test", "slot"]);
        assert_eq!(
            parse_command(&test_get_slot, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::GetSlot,
                signers: vec![],
            }
        );

        let test_total_supply = test_commands
            .clone()
            .get_matches_from(vec!["test", "total-supply"]);
        assert_eq!(
            parse_command(&test_total_supply, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::TotalSupply,
                signers: vec![],
            }
        );

        let test_transaction_count = test_commands
            .clone()
            .get_matches_from(vec!["test", "transaction-count"]);
        assert_eq!(
            parse_command(&test_transaction_count, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::GetTransactionCount,
                signers: vec![],
            }
        );

        let test_ping = test_commands.clone().get_matches_from(vec![
            "test",
            "ping",
            "-i",
            "1",
            "-c",
            "2",
            "-t",
            "3",
            "--commitment",
            "max",
        ]);
        assert_eq!(
            parse_command(&test_ping, &default_signer, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::Ping {
                    lamports: 1,
                    interval: Duration::from_secs(1),
                    count: Some(2),
                    timeout: Duration::from_secs(3),
                },
                signers: vec![default_keypair.into()],
            }
        );
    }
}
