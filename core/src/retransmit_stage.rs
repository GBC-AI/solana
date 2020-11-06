//! The `retransmit_stage` retransmits shreds between validators

use crate::{
    cluster_info::{compute_retransmit_peers, ClusterInfo, CFG as CLUSTER_CFG},
    cluster_info_vote_listener::VerifiedVoteReceiver,
    cluster_slots::ClusterSlots,
    cluster_slots_service::ClusterSlotsService,
    completed_data_sets_service::CompletedDataSetsSender,
    contact_info::ContactInfo,
    repair_service::DuplicateSlotsResetSender,
    repair_service::RepairInfo,
    result::{Error, Result},
    window_service::{should_retransmit_and_persist, WindowService},
};
use crossbeam_channel::Receiver;
use solana_ledger::{
    blockstore::{Blockstore, CompletedSlotsReceiver},
    leader_schedule_cache::LeaderScheduleCache,
    staking_utils,
};
use solana_measure::measure::Measure;
use solana_metrics::inc_new_counter_error;
use solana_perf::packet::Packets;
use solana_runtime::bank_forks::BankForks;
use solana_sdk::clock::{Epoch, Slot};
use solana_sdk::epoch_schedule::EpochSchedule;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::timing::timestamp;
use solana_streamer::streamer::PacketReceiver;
use std::{
    cmp,
    collections::hash_set::HashSet,
    collections::{BTreeMap, HashMap},
    net::UdpSocket,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::mpsc::channel,
    sync::mpsc::RecvTimeoutError,
    sync::Mutex,
    sync::{Arc, RwLock},
    thread::{self, Builder, JoinHandle},
    time::Duration,
};

// Limit a given thread to consume about this many packets so that
// it doesn't pull up too much work.
const MAX_PACKET_BATCH_SIZE: usize = 100;

#[derive(Default)]
struct RetransmitStats {
    total_packets: AtomicU64,
    total_batches: AtomicU64,
    total_time: AtomicU64,
    epoch_fetch: AtomicU64,
    epoch_cache_update: AtomicU64,
    repair_total: AtomicU64,
    discard_total: AtomicU64,
    retransmit_total: AtomicU64,
    last_ts: AtomicU64,
    compute_turbine_peers_total: AtomicU64,
    packets_by_slot: Mutex<BTreeMap<Slot, usize>>,
    packets_by_source: Mutex<BTreeMap<String, usize>>,
}

#[allow(clippy::too_many_arguments)]
fn update_retransmit_stats(
    stats: &Arc<RetransmitStats>,
    total_time: u64,
    total_packets: usize,
    retransmit_total: u64,
    discard_total: u64,
    repair_total: u64,
    compute_turbine_peers_total: u64,
    peers_len: usize,
    packets_by_slot: HashMap<Slot, usize>,
    packets_by_source: HashMap<String, usize>,
    epoch_fetch: u64,
    epoch_cach_update: u64,
) {
    stats.total_time.fetch_add(total_time, Ordering::Relaxed);
    stats
        .total_packets
        .fetch_add(total_packets as u64, Ordering::Relaxed);
    stats
        .retransmit_total
        .fetch_add(retransmit_total, Ordering::Relaxed);
    stats
        .repair_total
        .fetch_add(repair_total, Ordering::Relaxed);
    stats
        .discard_total
        .fetch_add(discard_total, Ordering::Relaxed);
    stats
        .compute_turbine_peers_total
        .fetch_add(compute_turbine_peers_total, Ordering::Relaxed);
    stats.total_batches.fetch_add(1, Ordering::Relaxed);
    stats.epoch_fetch.fetch_add(epoch_fetch, Ordering::Relaxed);
    stats
        .epoch_cache_update
        .fetch_add(epoch_cach_update, Ordering::Relaxed);
    {
        let mut stats_packets_by_slot = stats.packets_by_slot.lock().unwrap();
        for (slot, count) in packets_by_slot {
            *stats_packets_by_slot.entry(slot).or_insert(0) += count;
        }
    }
    {
        let mut stats_packets_by_source = stats.packets_by_source.lock().unwrap();
        for (source, count) in packets_by_source {
            *stats_packets_by_source.entry(source).or_insert(0) += count;
        }
    }

    let now = timestamp();
    let last = stats.last_ts.load(Ordering::Relaxed);
    if now - last > 2000 && stats.last_ts.compare_and_swap(last, now, Ordering::Relaxed) == last {
        datapoint_info!("retransmit-num_nodes", ("count", peers_len, i64));
        datapoint_info!(
            "retransmit-stage",
            (
                "total_time",
                stats.total_time.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "epoch_fetch",
                stats.epoch_fetch.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "epoch_cache_update",
                stats.epoch_cache_update.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "total_batches",
                stats.total_batches.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "total_packets",
                stats.total_packets.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "retransmit_total",
                stats.retransmit_total.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "compute_turbine",
                stats.compute_turbine_peers_total.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "repair_total",
                stats.repair_total.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "discard_total",
                stats.discard_total.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
        );
        let mut packets_by_slot = stats.packets_by_slot.lock().unwrap();
        info!("retransmit: packets_by_slot: {:?}", packets_by_slot);
        packets_by_slot.clear();
        drop(packets_by_slot);
        let mut packets_by_source = stats.packets_by_source.lock().unwrap();
        let mut top = BTreeMap::new();
        let mut max = 0;
        for (source, num) in packets_by_source.iter() {
            if *num > max {
                top.insert(*num, source.clone());
                if top.len() > 5 {
                    let last = *top.iter().next().unwrap().0;
                    top.remove(&last);
                }
                max = *top.iter().next().unwrap().0;
            }
        }
        info!(
            "retransmit: top packets_by_source: {:?} len: {}",
            top,
            packets_by_source.len()
        );
        packets_by_source.clear();
    }
}

#[derive(Default)]
struct EpochStakesCache {
    epoch: Epoch,
    stakes: Option<Arc<HashMap<Pubkey, u64>>>,
    peers: Vec<ContactInfo>,
    stakes_and_index: Vec<(u64, usize)>,
}

fn retransmit(
    bank_forks: &Arc<RwLock<BankForks>>,
    leader_schedule_cache: &Arc<LeaderScheduleCache>,
    cluster_info: &ClusterInfo,
    r: &Arc<Mutex<PacketReceiver>>,
    sock: &UdpSocket,
    id: u32,
    stats: &Arc<RetransmitStats>,
    epoch_stakes_cache: &Arc<RwLock<EpochStakesCache>>,
    last_peer_update: &Arc<AtomicU64>,
) -> Result<()> {
    let timer = Duration::new(1, 0);
    let r_lock = r.lock().unwrap();
    let packets = r_lock.recv_timeout(timer)?;
    let mut timer_start = Measure::start("retransmit");
    let mut total_packets = packets.packets.len();
    let mut packet_v = vec![packets];
    while let Ok(nq) = r_lock.try_recv() {
        total_packets += nq.packets.len();
        packet_v.push(nq);
        if total_packets >= MAX_PACKET_BATCH_SIZE {
            break;
        }
    }
    drop(r_lock);

    let mut epoch_fetch = Measure::start("retransmit_epoch_fetch");
    let r_bank = bank_forks.read().unwrap().working_bank();
    let bank_epoch = r_bank.get_leader_schedule_epoch(r_bank.slot());
    epoch_fetch.stop();

    let mut epoch_cache_update = Measure::start("retransmit_epoch_cach_update");
    let mut r_epoch_stakes_cache = epoch_stakes_cache.read().unwrap();
    if r_epoch_stakes_cache.epoch != bank_epoch {
        drop(r_epoch_stakes_cache);
        let mut w_epoch_stakes_cache = epoch_stakes_cache.write().unwrap();
        if w_epoch_stakes_cache.epoch != bank_epoch {
            let stakes = staking_utils::staked_nodes_at_epoch(&r_bank, bank_epoch);
            let stakes = stakes.map(Arc::new);
            w_epoch_stakes_cache.stakes = stakes;
            w_epoch_stakes_cache.epoch = bank_epoch;
        }
        drop(w_epoch_stakes_cache);
        r_epoch_stakes_cache = epoch_stakes_cache.read().unwrap();
    }

    let now = timestamp();
    let last = last_peer_update.load(Ordering::Relaxed);
    if now - last > 1000 && last_peer_update.compare_and_swap(last, now, Ordering::Relaxed) == last
    {
        drop(r_epoch_stakes_cache);
        let mut w_epoch_stakes_cache = epoch_stakes_cache.write().unwrap();
        let (peers, stakes_and_index) =
            cluster_info.sorted_retransmit_peers_and_stakes(w_epoch_stakes_cache.stakes.clone());
        w_epoch_stakes_cache.peers = peers;
        w_epoch_stakes_cache.stakes_and_index = stakes_and_index;
        drop(w_epoch_stakes_cache);
        r_epoch_stakes_cache = epoch_stakes_cache.read().unwrap();
    }
    let mut peers_len = 0;
    epoch_cache_update.stop();

    let my_id = cluster_info.id();
    let mut discard_total = 0;
    let mut repair_total = 0;
    let mut retransmit_total = 0;
    let mut compute_turbine_peers_total = 0;
    let mut packets_by_slot: HashMap<Slot, usize> = HashMap::new();
    let mut packets_by_source: HashMap<String, usize> = HashMap::new();
    for mut packets in packet_v {
        for packet in packets.packets.iter_mut() {
            // skip discarded packets and repair packets
            if packet.meta.discard {
                total_packets -= 1;
                discard_total += 1;
                continue;
            }
            if packet.meta.repair {
                total_packets -= 1;
                repair_total += 1;
                continue;
            }

            let mut compute_turbine_peers = Measure::start("turbine_start");
            let (my_index, mut shuffled_stakes_and_index) = ClusterInfo::shuffle_peers_and_index(
                &my_id,
                &r_epoch_stakes_cache.peers,
                &r_epoch_stakes_cache.stakes_and_index,
                packet.meta.seed,
            );
            peers_len = cmp::max(peers_len, shuffled_stakes_and_index.len());
            shuffled_stakes_and_index.remove(my_index);
            // split off the indexes, we don't need the stakes anymore
            let indexes = shuffled_stakes_and_index
                .into_iter()
                .map(|(_, index)| index)
                .collect();

            let (neighbors, children) =
                compute_retransmit_peers(CLUSTER_CFG.DATA_PLANE_FANOUT, my_index, indexes);
            let neighbors: Vec<_> = neighbors
                .into_iter()
                .map(|index| &r_epoch_stakes_cache.peers[index])
                .collect();
            let children: Vec<_> = children
                .into_iter()
                .map(|index| &r_epoch_stakes_cache.peers[index])
                .collect();
            compute_turbine_peers.stop();
            compute_turbine_peers_total += compute_turbine_peers.as_us();

            *packets_by_slot.entry(packet.meta.slot).or_insert(0) += 1;
            *packets_by_source
                .entry(packet.meta.addr().to_string())
                .or_insert(0) += 1;

            let leader =
                leader_schedule_cache.slot_leader_at(packet.meta.slot, Some(r_bank.as_ref()));
            let mut retransmit_time = Measure::start("retransmit_to");
            if !packet.meta.forward {
                ClusterInfo::retransmit_to(&neighbors, packet, leader, sock, true)?;
                ClusterInfo::retransmit_to(&children, packet, leader, sock, false)?;
            } else {
                ClusterInfo::retransmit_to(&children, packet, leader, sock, true)?;
            }
            retransmit_time.stop();
            retransmit_total += retransmit_time.as_us();
        }
    }
    timer_start.stop();
    debug!(
        "retransmitted {} packets in {}ms retransmit_time: {}ms id: {}",
        total_packets,
        timer_start.as_ms(),
        retransmit_total,
        id,
    );
    update_retransmit_stats(
        stats,
        timer_start.as_us(),
        total_packets,
        retransmit_total,
        discard_total,
        repair_total,
        compute_turbine_peers_total,
        peers_len,
        packets_by_slot,
        packets_by_source,
        epoch_fetch.as_us(),
        epoch_cache_update.as_us(),
    );

    Ok(())
}

/// Service to retransmit messages from the leader or layer 1 to relevant peer nodes.
/// See `cluster_info` for network layer definitions.
/// # Arguments
/// * `sockets` - Sockets to read from.
/// * `bank_forks` - The BankForks structure
/// * `leader_schedule_cache` - The leader schedule to verify shreds
/// * `cluster_info` - This structure needs to be updated and populated by the bank and via gossip.
/// * `r` - Receive channel for shreds to be retransmitted to all the layer 1 nodes.
pub fn retransmitter(
    sockets: Arc<Vec<UdpSocket>>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: &Arc<LeaderScheduleCache>,
    cluster_info: Arc<ClusterInfo>,
    r: Arc<Mutex<PacketReceiver>>,
) -> Vec<JoinHandle<()>> {
    let stats = Arc::new(RetransmitStats::default());
    (0..sockets.len())
        .map(|s| {
            let sockets = sockets.clone();
            let bank_forks = bank_forks.clone();
            let leader_schedule_cache = leader_schedule_cache.clone();
            let r = r.clone();
            let cluster_info = cluster_info.clone();
            let stats = stats.clone();
            let epoch_stakes_cache = Arc::new(RwLock::new(EpochStakesCache::default()));
            let last_peer_update = Arc::new(AtomicU64::new(0));

            Builder::new()
                .name("solana-retransmitter".to_string())
                .spawn(move || {
                    trace!("retransmitter started");
                    loop {
                        if let Err(e) = retransmit(
                            &bank_forks,
                            &leader_schedule_cache,
                            &cluster_info,
                            &r,
                            &sockets[s],
                            s as u32,
                            &stats,
                            &epoch_stakes_cache,
                            &last_peer_update,
                        ) {
                            match e {
                                Error::RecvTimeoutError(RecvTimeoutError::Disconnected) => break,
                                Error::RecvTimeoutError(RecvTimeoutError::Timeout) => (),
                                _ => {
                                    inc_new_counter_error!("streamer-retransmit-error", 1, 1);
                                }
                            }
                        }
                    }
                    trace!("exiting retransmitter");
                })
                .unwrap()
        })
        .collect()
}

pub struct RetransmitStage {
    thread_hdls: Vec<JoinHandle<()>>,
    window_service: WindowService,
    cluster_slots_service: ClusterSlotsService,
}

impl RetransmitStage {
    #[allow(clippy::new_ret_no_self)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        blockstore: Arc<Blockstore>,
        cluster_info: &Arc<ClusterInfo>,
        retransmit_sockets: Arc<Vec<UdpSocket>>,
        repair_socket: Arc<UdpSocket>,
        verified_receiver: Receiver<Vec<Packets>>,
        exit: &Arc<AtomicBool>,
        completed_slots_receiver: CompletedSlotsReceiver,
        epoch_schedule: EpochSchedule,
        cfg: Option<Arc<AtomicBool>>,
        shred_version: u16,
        cluster_slots: Arc<ClusterSlots>,
        duplicate_slots_reset_sender: DuplicateSlotsResetSender,
        verified_vote_receiver: VerifiedVoteReceiver,
        repair_validators: Option<HashSet<Pubkey>>,
        completed_data_sets_sender: CompletedDataSetsSender,
    ) -> Self {
        let (retransmit_sender, retransmit_receiver) = channel();

        let retransmit_receiver = Arc::new(Mutex::new(retransmit_receiver));
        let t_retransmit = retransmitter(
            retransmit_sockets,
            bank_forks.clone(),
            leader_schedule_cache,
            cluster_info.clone(),
            retransmit_receiver,
        );

        let leader_schedule_cache_clone = leader_schedule_cache.clone();
        let cluster_slots_service = ClusterSlotsService::new(
            blockstore.clone(),
            cluster_slots.clone(),
            bank_forks.clone(),
            cluster_info.clone(),
            completed_slots_receiver,
            exit.clone(),
        );
        let repair_info = RepairInfo {
            bank_forks,
            epoch_schedule,
            duplicate_slots_reset_sender,
            repair_validators,
        };
        let window_service = WindowService::new(
            blockstore,
            cluster_info.clone(),
            verified_receiver,
            retransmit_sender,
            repair_socket,
            exit,
            repair_info,
            leader_schedule_cache,
            move |id, shred, working_bank, last_root| {
                let is_connected = cfg
                    .as_ref()
                    .map(|x| x.load(Ordering::Relaxed))
                    .unwrap_or(true);
                let rv = should_retransmit_and_persist(
                    shred,
                    working_bank,
                    &leader_schedule_cache_clone,
                    id,
                    last_root,
                    shred_version,
                );
                rv && is_connected
            },
            cluster_slots,
            verified_vote_receiver,
            completed_data_sets_sender,
        );

        let thread_hdls = t_retransmit;
        Self {
            thread_hdls,
            window_service,
            cluster_slots_service,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        self.window_service.join()?;
        self.cluster_slots_service.join()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contact_info::ContactInfo;
    use solana_ledger::blockstore_processor::{process_blockstore, ProcessOptions};
    use solana_ledger::create_new_tmp_ledger;
    use solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo};
    use solana_net_utils::find_available_port_in_range;
    use solana_perf::packet::{Meta, Packet, Packets};
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_skip_repair() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(123);
        let (ledger_path, _blockhash) = create_new_tmp_ledger!(&genesis_config);
        let blockstore = Blockstore::open(&ledger_path).unwrap();
        let opts = ProcessOptions {
            full_leader_cache: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, cached_leader_schedule) =
            process_blockstore(&genesis_config, &blockstore, Vec::new(), opts).unwrap();
        let leader_schedule_cache = Arc::new(cached_leader_schedule);
        let bank_forks = Arc::new(RwLock::new(bank_forks));

        let mut me = ContactInfo::new_localhost(&solana_sdk::pubkey::new_rand(), 0);
        let ip_addr = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
        let port = find_available_port_in_range(ip_addr, (8000, 10000)).unwrap();
        let me_retransmit = UdpSocket::bind(format!("127.0.0.1:{}", port)).unwrap();
        // need to make sure tvu and tpu are valid addresses
        me.tvu_forwards = me_retransmit.local_addr().unwrap();
        let port = find_available_port_in_range(ip_addr, (8000, 10000)).unwrap();
        me.tvu = UdpSocket::bind(format!("127.0.0.1:{}", port))
            .unwrap()
            .local_addr()
            .unwrap();

        let other = ContactInfo::new_localhost(&solana_sdk::pubkey::new_rand(), 0);
        let cluster_info = ClusterInfo::new_with_invalid_keypair(other);
        cluster_info.insert_info(me);

        let retransmit_socket = Arc::new(vec![UdpSocket::bind("0.0.0.0:0").unwrap()]);
        let cluster_info = Arc::new(cluster_info);

        let (retransmit_sender, retransmit_receiver) = channel();
        let t_retransmit = retransmitter(
            retransmit_socket,
            bank_forks,
            &leader_schedule_cache,
            cluster_info,
            Arc::new(Mutex::new(retransmit_receiver)),
        );
        let _thread_hdls = vec![t_retransmit];

        let packets = Packets::new(vec![Packet::default()]);
        // it should send this over the sockets.
        retransmit_sender.send(packets).unwrap();
        let mut packets = Packets::new(vec![]);
        solana_streamer::packet::recv_from(&mut packets, &me_retransmit, 1).unwrap();
        assert_eq!(packets.packets.len(), 1);
        assert_eq!(packets.packets[0].meta.repair, false);

        let repair = Packet {
            meta: Meta {
                repair: true,
                ..Meta::default()
            },
            ..Packet::default()
        };

        // send 1 repair and 1 "regular" packet so that we don't block forever on the recv_from
        let packets = Packets::new(vec![repair, Packet::default()]);
        retransmit_sender.send(packets).unwrap();
        let mut packets = Packets::new(vec![]);
        solana_streamer::packet::recv_from(&mut packets, &me_retransmit, 1).unwrap();
        assert_eq!(packets.packets.len(), 1);
        assert_eq!(packets.packets[0].meta.repair, false);
    }
}
