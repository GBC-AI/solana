use crate::{
    cluster_info::{ClusterInfo, CFG},
    crds_value::CrdsValueLabel,
    optimistic_confirmation_verifier::OptimisticConfirmationVerifier,
    optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSender},
    poh_recorder::PohRecorder,
    pubkey_references::LockedPubkeyReferences,
    result::{Error, Result},
    rpc_subscriptions::RpcSubscriptions,
    sigverify,
    verified_vote_packets::VerifiedVotePackets,
    vote_stake_tracker::VoteStakeTracker,
};
use crossbeam_channel::{
    unbounded, Receiver as CrossbeamReceiver, RecvTimeoutError, Select, Sender as CrossbeamSender,
};
use itertools::izip;
use log::*;
use solana_ledger::blockstore::Blockstore;
use solana_metrics::inc_new_counter_debug;
use solana_perf::packet::{self, Packets};
use solana_runtime::{
    bank::Bank,
    bank_forks::BankForks,
    epoch_stakes::{EpochAuthorizedVoters, EpochStakes},
    stakes::Stakes,
    vote_sender_types::{ReplayVoteReceiver, ReplayedVote},
};
use solana_sdk::{
    clock::{Epoch, Slot, DEFAULT_MS_PER_SLOT},
    epoch_schedule::EpochSchedule,
    hash::Hash,
    pubkey::Pubkey,
    transaction::Transaction,
};
use solana_vote_program::{self, vote_state::Vote, vote_transaction};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        {Arc, Mutex, RwLock},
    },
    thread::{self, sleep, Builder, JoinHandle},
    time::{Duration, Instant},
};

// Map from a vote account to the authorized voter for an epoch
pub type VerifiedLabelVotePacketsSender = CrossbeamSender<Vec<(CrdsValueLabel, Packets)>>;
pub type VerifiedLabelVotePacketsReceiver = CrossbeamReceiver<Vec<(CrdsValueLabel, Packets)>>;
pub type VerifiedVoteTransactionsSender = CrossbeamSender<Vec<Transaction>>;
pub type VerifiedVoteTransactionsReceiver = CrossbeamReceiver<Vec<Transaction>>;
pub type VerifiedVoteSender = CrossbeamSender<(Pubkey, Vec<Slot>)>;
pub type VerifiedVoteReceiver = CrossbeamReceiver<(Pubkey, Vec<Slot>)>;

#[derive(Default)]
pub struct SlotVoteTracker {
    // Maps pubkeys that have voted for this slot
    // to whether or not we've seen the vote on gossip.
    // True if seen on gossip, false if only seen in replay.
    voted: HashMap<Arc<Pubkey>, bool>,
    optimistic_votes_tracker: HashMap<Hash, VoteStakeTracker>,
    updates: Option<Vec<Arc<Pubkey>>>,
    gossip_only_stake: u64,
}

impl SlotVoteTracker {
    #[allow(dead_code)]
    pub fn get_updates(&mut self) -> Option<Vec<Arc<Pubkey>>> {
        self.updates.take()
    }

    pub fn get_or_insert_optimistic_votes_tracker(&mut self, hash: Hash) -> &mut VoteStakeTracker {
        self.optimistic_votes_tracker.entry(hash).or_default()
    }
    pub fn optimistic_votes_tracker(&self, hash: &Hash) -> Option<&VoteStakeTracker> {
        self.optimistic_votes_tracker.get(hash)
    }
}

#[derive(Default)]
pub struct VoteTracker {
    // Map from a slot to a set of validators who have voted for that slot
    slot_vote_trackers: RwLock<HashMap<Slot, Arc<RwLock<SlotVoteTracker>>>>,
    // Don't track votes from people who are not staked, acts as a spam filter
    epoch_authorized_voters: RwLock<HashMap<Epoch, Arc<EpochAuthorizedVoters>>>,
    leader_schedule_epoch: RwLock<Epoch>,
    current_epoch: RwLock<Epoch>,
    keys: LockedPubkeyReferences,
    epoch_schedule: EpochSchedule,
}

impl VoteTracker {
    pub fn new(root_bank: &Bank) -> Self {
        let current_epoch = root_bank.epoch();
        let vote_tracker = Self {
            leader_schedule_epoch: RwLock::new(current_epoch),
            current_epoch: RwLock::new(current_epoch),
            epoch_schedule: *root_bank.epoch_schedule(),
            ..VoteTracker::default()
        };
        vote_tracker.progress_with_new_root_bank(&root_bank);
        assert_eq!(
            *vote_tracker.leader_schedule_epoch.read().unwrap(),
            root_bank.get_leader_schedule_epoch(root_bank.slot())
        );
        assert_eq!(*vote_tracker.current_epoch.read().unwrap(), current_epoch,);
        vote_tracker
    }

    pub fn get_or_insert_slot_tracker(&self, slot: Slot) -> Arc<RwLock<SlotVoteTracker>> {
        let mut slot_tracker = self.slot_vote_trackers.read().unwrap().get(&slot).cloned();

        if slot_tracker.is_none() {
            let new_slot_tracker = Arc::new(RwLock::new(SlotVoteTracker {
                voted: HashMap::new(),
                optimistic_votes_tracker: HashMap::default(),
                updates: None,
                gossip_only_stake: 0,
            }));
            self.slot_vote_trackers
                .write()
                .unwrap()
                .insert(slot, new_slot_tracker.clone());
            slot_tracker = Some(new_slot_tracker);
        }

        slot_tracker.unwrap()
    }

    pub fn get_slot_vote_tracker(&self, slot: Slot) -> Option<Arc<RwLock<SlotVoteTracker>>> {
        self.slot_vote_trackers.read().unwrap().get(&slot).cloned()
    }

    pub fn get_authorized_voter(&self, pubkey: &Pubkey, slot: Slot) -> Option<Pubkey> {
        let epoch = self.epoch_schedule.get_epoch(slot);
        self.epoch_authorized_voters
            .read()
            .unwrap()
            .get(&epoch)
            .map(|epoch_authorized_voters| epoch_authorized_voters.get(pubkey))
            .unwrap_or(None)
            .cloned()
    }

    pub fn vote_contains_authorized_voter(
        vote_tx: &Transaction,
        authorized_voter: &Pubkey,
    ) -> bool {
        let message = &vote_tx.message;
        for (i, key) in message.account_keys.iter().enumerate() {
            if message.is_signer(i) && key == authorized_voter {
                return true;
            }
        }

        false
    }

    #[cfg(test)]
    pub fn insert_vote(&self, slot: Slot, pubkey: Arc<Pubkey>) {
        let mut w_slot_vote_trackers = self.slot_vote_trackers.write().unwrap();

        let slot_vote_tracker = w_slot_vote_trackers.entry(slot).or_default();

        let mut w_slot_vote_tracker = slot_vote_tracker.write().unwrap();

        w_slot_vote_tracker.voted.insert(pubkey.clone(), true);
        if let Some(ref mut updates) = w_slot_vote_tracker.updates {
            updates.push(pubkey.clone())
        } else {
            w_slot_vote_tracker.updates = Some(vec![pubkey.clone()]);
        }

        self.keys.get_or_insert(&pubkey);
    }

    fn progress_leader_schedule_epoch(&self, root_bank: &Bank) {
        // Update with any newly calculated epoch state about future epochs
        let start_leader_schedule_epoch = *self.leader_schedule_epoch.read().unwrap();
        let mut greatest_leader_schedule_epoch = start_leader_schedule_epoch;
        for leader_schedule_epoch in
            start_leader_schedule_epoch..=root_bank.get_leader_schedule_epoch(root_bank.slot())
        {
            let exists = self
                .epoch_authorized_voters
                .read()
                .unwrap()
                .contains_key(&leader_schedule_epoch);
            if !exists {
                let epoch_authorized_voters = root_bank
                    .epoch_stakes(leader_schedule_epoch)
                    .unwrap()
                    .epoch_authorized_voters()
                    .clone();
                self.epoch_authorized_voters
                    .write()
                    .unwrap()
                    .insert(leader_schedule_epoch, epoch_authorized_voters);
                greatest_leader_schedule_epoch = leader_schedule_epoch;
            }
        }

        if greatest_leader_schedule_epoch != start_leader_schedule_epoch {
            *self.leader_schedule_epoch.write().unwrap() = greatest_leader_schedule_epoch;
        }
    }

    fn purge_stale_state(&self, root_bank: &Bank) {
        // Purge any outdated slot data
        let new_root = root_bank.slot();
        let root_epoch = root_bank.epoch();
        self.slot_vote_trackers
            .write()
            .unwrap()
            .retain(|slot, _| *slot >= new_root);

        let current_epoch = *self.current_epoch.read().unwrap();
        if root_epoch != current_epoch {
            // If root moved to a new epoch, purge outdated state
            self.epoch_authorized_voters
                .write()
                .unwrap()
                .retain(|epoch, _| *epoch >= root_epoch);
            self.keys.purge();
            *self.current_epoch.write().unwrap() = root_epoch;
        }
    }

    fn progress_with_new_root_bank(&self, root_bank: &Bank) {
        self.progress_leader_schedule_epoch(root_bank);
        self.purge_stale_state(root_bank);
    }
}

pub struct ClusterInfoVoteListener {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl ClusterInfoVoteListener {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        exit: &Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        verified_packets_sender: CrossbeamSender<Vec<Packets>>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        subscriptions: Arc<RpcSubscriptions>,
        verified_vote_sender: VerifiedVoteSender,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        bank_notification_sender: Option<BankNotificationSender>,
    ) -> Self {
        let exit_ = exit.clone();

        let (verified_vote_label_packets_sender, verified_vote_label_packets_receiver) =
            unbounded();
        let (verified_vote_transactions_sender, verified_vote_transactions_receiver) = unbounded();
        let listen_thread = Builder::new()
            .name("solana-cluster_info_vote_listener".to_string())
            .spawn(move || {
                let _ = Self::recv_loop(
                    exit_,
                    &cluster_info,
                    verified_vote_label_packets_sender,
                    verified_vote_transactions_sender,
                );
            })
            .unwrap();

        let exit_ = exit.clone();
        let poh_recorder = poh_recorder.clone();
        let bank_send_thread = Builder::new()
            .name("solana-cluster_info_bank_send".to_string())
            .spawn(move || {
                let _ = Self::bank_send_loop(
                    exit_,
                    verified_vote_label_packets_receiver,
                    poh_recorder,
                    &verified_packets_sender,
                );
            })
            .unwrap();

        let exit_ = exit.clone();
        let send_thread = Builder::new()
            .name("solana-cluster_info_process_votes".to_string())
            .spawn(move || {
                let _ = Self::process_votes_loop(
                    exit_,
                    verified_vote_transactions_receiver,
                    vote_tracker,
                    bank_forks,
                    subscriptions,
                    verified_vote_sender,
                    replay_votes_receiver,
                    blockstore,
                    bank_notification_sender,
                );
            })
            .unwrap();

        Self {
            thread_hdls: vec![listen_thread, send_thread, bank_send_thread],
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }

    fn recv_loop(
        exit: Arc<AtomicBool>,
        cluster_info: &ClusterInfo,
        verified_vote_label_packets_sender: VerifiedLabelVotePacketsSender,
        verified_vote_transactions_sender: VerifiedVoteTransactionsSender,
    ) -> Result<()> {
        let mut last_ts = 0;
        loop {
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }
            let (labels, votes, new_ts) = cluster_info.get_votes(last_ts);
            inc_new_counter_debug!("cluster_info_vote_listener-recv_count", votes.len());

            last_ts = new_ts;
            if !votes.is_empty() {
                let (vote_txs, packets) = Self::verify_votes(votes, labels);
                verified_vote_transactions_sender.send(vote_txs)?;
                verified_vote_label_packets_sender.send(packets)?;
            }

            sleep(Duration::from_millis(CFG.GOSSIP_SLEEP_MILLIS));
        }
    }

    fn verify_votes(
        votes: Vec<Transaction>,
        labels: Vec<CrdsValueLabel>,
    ) -> (Vec<Transaction>, Vec<(CrdsValueLabel, Packets)>) {
        let msgs = packet::to_packets_chunked(&votes, 1);
        let r = sigverify::ed25519_verify_cpu(&msgs);

        assert_eq!(
            r.iter()
                .map(|packets_results| packets_results.len())
                .sum::<usize>(),
            votes.len()
        );

        let (vote_txs, packets) = izip!(
            labels.into_iter(),
            votes.into_iter(),
            r.iter().flatten(),
            msgs,
        )
        .filter_map(|(label, vote, verify_result, packet)| {
            if *verify_result != 0 {
                Some((vote, (label, packet)))
            } else {
                None
            }
        })
        .unzip();
        (vote_txs, packets)
    }

    fn bank_send_loop(
        exit: Arc<AtomicBool>,
        verified_vote_label_packets_receiver: VerifiedLabelVotePacketsReceiver,
        poh_recorder: Arc<Mutex<PohRecorder>>,
        verified_packets_sender: &CrossbeamSender<Vec<Packets>>,
    ) -> Result<()> {
        let mut verified_vote_packets = VerifiedVotePackets::default();
        let mut time_since_lock = Instant::now();
        let mut update_version = 0;
        loop {
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            if let Err(e) = verified_vote_packets.get_and_process_vote_packets(
                &verified_vote_label_packets_receiver,
                &mut update_version,
            ) {
                match e {
                    Error::CrossbeamRecvTimeoutError(RecvTimeoutError::Disconnected) => {
                        return Ok(());
                    }
                    Error::CrossbeamRecvTimeoutError(RecvTimeoutError::Timeout) => (),
                    _ => {
                        error!("thread {:?} error {:?}", thread::current().name(), e);
                    }
                }
            }

            if time_since_lock.elapsed().as_millis() > CFG.GOSSIP_SLEEP_MILLIS as u128 {
                let bank = poh_recorder.lock().unwrap().bank();
                if let Some(bank) = bank {
                    let last_version = bank.last_vote_sync.load(Ordering::Relaxed);
                    let (new_version, msgs) = verified_vote_packets.get_latest_votes(last_version);
                    verified_packets_sender.send(msgs)?;
                    bank.last_vote_sync.compare_and_swap(
                        last_version,
                        new_version,
                        Ordering::Relaxed,
                    );
                    time_since_lock = Instant::now();
                }
            }
        }
    }

    fn process_votes_loop(
        exit: Arc<AtomicBool>,
        gossip_vote_txs_receiver: VerifiedVoteTransactionsReceiver,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        subscriptions: Arc<RpcSubscriptions>,
        verified_vote_sender: VerifiedVoteSender,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        bank_notification_sender: Option<BankNotificationSender>,
    ) -> Result<()> {
        let mut confirmation_verifier =
            OptimisticConfirmationVerifier::new(bank_forks.read().unwrap().root());
        let mut last_process_root = Instant::now();
        loop {
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            let root_bank = bank_forks.read().unwrap().root_bank().clone();
            if last_process_root.elapsed().as_millis() > DEFAULT_MS_PER_SLOT as u128 {
                let unrooted_optimistic_slots = confirmation_verifier
                    .verify_for_unrooted_optimistic_slots(&root_bank, &blockstore);
                // SlotVoteTracker's for all `slots` in `unrooted_optimistic_slots`
                // should still be available because we haven't purged in
                // `progress_with_new_root_bank()` yet, which is called below
                OptimisticConfirmationVerifier::log_unrooted_optimistic_slots(
                    &root_bank,
                    &vote_tracker,
                    &unrooted_optimistic_slots,
                );
                vote_tracker.progress_with_new_root_bank(&root_bank);
                last_process_root = Instant::now();
            }
            let confirmed_slots = Self::listen_and_confirm_votes(
                &gossip_vote_txs_receiver,
                &vote_tracker,
                &root_bank,
                &subscriptions,
                &verified_vote_sender,
                &replay_votes_receiver,
                &bank_notification_sender,
            );
            match confirmed_slots {
                Ok(confirmed_slots) => {
                    confirmation_verifier.add_new_optimistic_confirmed_slots(confirmed_slots);
                }
                Err(e) => match e {
                    Error::CrossbeamRecvTimeoutError(RecvTimeoutError::Timeout)
                    | Error::ReadyTimeoutError => (),
                    _ => {
                        error!("thread {:?} error {:?}", thread::current().name(), e);
                    }
                },
            }
        }
    }

    #[cfg(test)]
    pub fn get_and_process_votes_for_tests(
        gossip_vote_txs_receiver: &VerifiedVoteTransactionsReceiver,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        verified_vote_sender: &VerifiedVoteSender,
        replay_votes_receiver: &ReplayVoteReceiver,
    ) -> Result<Vec<(Slot, Hash)>> {
        Self::listen_and_confirm_votes(
            gossip_vote_txs_receiver,
            vote_tracker,
            root_bank,
            subscriptions,
            verified_vote_sender,
            replay_votes_receiver,
            &None,
        )
    }

    fn listen_and_confirm_votes(
        gossip_vote_txs_receiver: &VerifiedVoteTransactionsReceiver,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        verified_vote_sender: &VerifiedVoteSender,
        replay_votes_receiver: &ReplayVoteReceiver,
        bank_notification_sender: &Option<BankNotificationSender>,
    ) -> Result<Vec<(Slot, Hash)>> {
        let mut sel = Select::new();
        sel.recv(gossip_vote_txs_receiver);
        sel.recv(replay_votes_receiver);
        let mut remaining_wait_time = 200;
        loop {
            if remaining_wait_time == 0 {
                break;
            }
            let start = Instant::now();
            // Wait for one of the receivers to be ready. `ready_timeout`
            // will return if channels either have something, or are
            // disconnected. `ready_timeout` can wake up spuriously,
            // hence the loop
            let _ = sel.ready_timeout(Duration::from_millis(remaining_wait_time))?;

            // Should not early return from this point onwards until `process_votes()`
            // returns below to avoid missing any potential `optimistic_confirmed_slots`
            let gossip_vote_txs: Vec<_> = gossip_vote_txs_receiver.try_iter().flatten().collect();
            let replay_votes: Vec<_> = replay_votes_receiver.try_iter().collect();
            if !gossip_vote_txs.is_empty() || !replay_votes.is_empty() {
                return Ok(Self::filter_and_confirm_with_new_votes(
                    vote_tracker,
                    gossip_vote_txs,
                    replay_votes,
                    root_bank,
                    subscriptions,
                    verified_vote_sender,
                    bank_notification_sender,
                ));
            } else {
                remaining_wait_time = remaining_wait_time
                    .saturating_sub(std::cmp::max(start.elapsed().as_millis() as u64, 1));
            }
        }
        Ok(vec![])
    }

    #[allow(clippy::too_many_arguments)]
    fn track_new_votes_and_notify_confirmations(
        vote: Vote,
        vote_pubkey: &Pubkey,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        verified_vote_sender: &VerifiedVoteSender,
        diff: &mut HashMap<Slot, HashMap<Arc<Pubkey>, bool>>,
        new_optimistic_confirmed_slots: &mut Vec<(Slot, Hash)>,
        is_gossip_vote: bool,
        bank_notification_sender: &Option<BankNotificationSender>,
    ) {
        if vote.slots.is_empty() {
            return;
        }

        let last_vote_slot = *vote.slots.last().unwrap();
        let last_vote_hash = vote.hash;

        let root = root_bank.slot();
        let mut is_new_vote = false;
        // If slot is before the root, ignore it
        for slot in vote.slots.iter().filter(|slot| **slot > root).rev() {
            let slot = *slot;

            // if we don't have stake information, ignore it
            let epoch = root_bank.epoch_schedule().get_epoch(slot);
            let epoch_stakes = root_bank.epoch_stakes(epoch);
            if epoch_stakes.is_none() {
                continue;
            }
            let epoch_stakes = epoch_stakes.unwrap();
            let unduplicated_pubkey = vote_tracker.keys.get_or_insert(&vote_pubkey);

            // The last vote slot, which is the greatest slot in the stack
            // of votes in a vote transaction, qualifies for optimistic confirmation.
            if slot == last_vote_slot {
                let vote_accounts = Stakes::vote_accounts(epoch_stakes.stakes());
                let stake = vote_accounts
                    .get(&vote_pubkey)
                    .map(|(stake, _)| *stake)
                    .unwrap_or_default();
                let total_stake = epoch_stakes.total_stake();

                // Fast track processing of the last slot in a vote transactions
                // so that notifications for optimistic confirmation can be sent
                // as soon as possible.
                let (is_confirmed, is_new) = Self::track_optimistic_confirmation_vote(
                    vote_tracker,
                    last_vote_slot,
                    last_vote_hash,
                    unduplicated_pubkey.clone(),
                    stake,
                    total_stake,
                );

                if is_confirmed {
                    new_optimistic_confirmed_slots.push((last_vote_slot, last_vote_hash));
                    // Notify subscribers about new optimistic confirmation
                    if let Some(sender) = bank_notification_sender {
                        sender
                            .send(BankNotification::OptimisticallyConfirmed(last_vote_slot))
                            .unwrap_or_else(|err| {
                                warn!("bank_notification_sender failed: {:?}", err)
                            });
                    }
                }

                if !is_new && !is_gossip_vote {
                    // By now:
                    // 1) The vote must have come from ReplayStage,
                    // 2) We've seen this vote from replay for this hash before
                    // (`track_optimistic_confirmation_vote()` will not set `is_new == true`
                    // for same slot different hash), so short circuit because this vote
                    // has no new information

                    // Note gossip votes will always be processed because those should be unique
                    // and we need to update the gossip-only stake in the `VoteTracker`.
                    return;
                }

                is_new_vote = is_new;
            }

            diff.entry(slot)
                .or_default()
                .entry(unduplicated_pubkey)
                .and_modify(|seen_in_gossip_previously| {
                    *seen_in_gossip_previously = *seen_in_gossip_previously || is_gossip_vote
                })
                .or_insert(is_gossip_vote);
        }

        if is_new_vote {
            subscriptions.notify_vote(&vote);
            let _ = verified_vote_sender.send((*vote_pubkey, vote.slots));
        }
    }

    fn filter_gossip_votes(
        vote_tracker: &VoteTracker,
        vote_pubkey: &Pubkey,
        vote: &Vote,
        gossip_tx: &Transaction,
    ) -> bool {
        if vote.slots.is_empty() {
            return false;
        }
        let last_vote_slot = vote.slots.last().unwrap();
        // Votes from gossip need to be verified as they have not been
        // verified by the replay pipeline. Determine the authorized voter
        // based on the last vote slot. This will  drop votes from authorized
        // voters trying to make votes for slots earlier than the epoch for
        // which they are authorized
        let actual_authorized_voter =
            vote_tracker.get_authorized_voter(&vote_pubkey, *last_vote_slot);

        if actual_authorized_voter.is_none() {
            return false;
        }

        // Voting without the correct authorized pubkey, dump the vote
        if !VoteTracker::vote_contains_authorized_voter(
            &gossip_tx,
            &actual_authorized_voter.unwrap(),
        ) {
            return false;
        }

        true
    }

    fn filter_and_confirm_with_new_votes(
        vote_tracker: &VoteTracker,
        gossip_vote_txs: Vec<Transaction>,
        replayed_votes: Vec<ReplayedVote>,
        root_bank: &Bank,
        subscriptions: &RpcSubscriptions,
        verified_vote_sender: &VerifiedVoteSender,
        bank_notification_sender: &Option<BankNotificationSender>,
    ) -> Vec<(Slot, Hash)> {
        let mut diff: HashMap<Slot, HashMap<Arc<Pubkey>, bool>> = HashMap::new();
        let mut new_optimistic_confirmed_slots = vec![];

        // Process votes from gossip and ReplayStage
        for (is_gossip, (vote_pubkey, vote, _)) in gossip_vote_txs
            .iter()
            .filter_map(|gossip_tx| {
                vote_transaction::parse_vote_transaction(gossip_tx)
                    .filter(|(vote_pubkey, vote, _)| {
                        Self::filter_gossip_votes(vote_tracker, vote_pubkey, vote, gossip_tx)
                    })
                    .map(|v| (true, v))
            })
            .chain(replayed_votes.into_iter().map(|v| (false, v)))
        {
            Self::track_new_votes_and_notify_confirmations(
                vote,
                &vote_pubkey,
                &vote_tracker,
                root_bank,
                subscriptions,
                verified_vote_sender,
                &mut diff,
                &mut new_optimistic_confirmed_slots,
                is_gossip,
                bank_notification_sender,
            );
        }

        // Process all the slots accumulated from replay and gossip.
        for (slot, mut slot_diff) in diff {
            let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
            {
                let r_slot_tracker = slot_tracker.read().unwrap();
                // Only keep the pubkeys we haven't seen voting for this slot
                slot_diff.retain(|pubkey, seen_in_gossip_above| {
                    let seen_in_gossip_previously = r_slot_tracker.voted.get(pubkey);
                    let is_new = seen_in_gossip_previously.is_none();
                    // `is_new_from_gossip` means we observed a vote for this slot
                    // for the first time in gossip
                    let is_new_from_gossip = !seen_in_gossip_previously.cloned().unwrap_or(false)
                        && *seen_in_gossip_above;
                    is_new || is_new_from_gossip
                });
            }
            let mut w_slot_tracker = slot_tracker.write().unwrap();
            if w_slot_tracker.updates.is_none() {
                w_slot_tracker.updates = Some(vec![]);
            }
            let mut gossip_only_stake = 0;
            let epoch = root_bank.epoch_schedule().get_epoch(slot);
            let epoch_stakes = root_bank.epoch_stakes(epoch);

            for (pubkey, seen_in_gossip_above) in slot_diff {
                if seen_in_gossip_above {
                    // By this point we know if the vote was seen in gossip above,
                    // it was not seen in gossip at any point in the past (if it was seen
                    // in gossip in the past, `is_new` would be false and it would have
                    // been filtered out above), so it's safe to increment the gossip-only
                    // stake
                    Self::sum_stake(&mut gossip_only_stake, epoch_stakes, &pubkey);
                }

                // From the `slot_diff.retain` earlier, we know because there are
                // no other writers to `slot_vote_tracker` that
                // `is_new || is_new_from_gossip`. In both cases we want to record
                // `is_new_from_gossip` for the `pubkey` entry.
                w_slot_tracker
                    .voted
                    .insert(pubkey.clone(), seen_in_gossip_above);
                w_slot_tracker.updates.as_mut().unwrap().push(pubkey);
            }

            w_slot_tracker.gossip_only_stake += gossip_only_stake
        }
        new_optimistic_confirmed_slots
    }

    // Returns if the slot was optimistically confirmed, and whether
    // the slot was new
    fn track_optimistic_confirmation_vote(
        vote_tracker: &VoteTracker,
        slot: Slot,
        hash: Hash,
        pubkey: Arc<Pubkey>,
        stake: u64,
        total_epoch_stake: u64,
    ) -> (bool, bool) {
        let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
        // Insert vote and check for optimistic confirmation
        let mut w_slot_tracker = slot_tracker.write().unwrap();

        w_slot_tracker
            .get_or_insert_optimistic_votes_tracker(hash)
            .add_vote_pubkey(pubkey, stake, total_epoch_stake)
    }

    fn sum_stake(sum: &mut u64, epoch_stakes: Option<&EpochStakes>, pubkey: &Pubkey) {
        if let Some(stakes) = epoch_stakes {
            if let Some(vote_account) = stakes.stakes().vote_accounts().get(pubkey) {
                *sum += vote_account.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank;
    use solana_perf::packet;
    use solana_runtime::{
        bank::Bank,
        commitment::BlockCommitmentCache,
        genesis_utils::{self, GenesisConfigInfo, ValidatorVoteKeypairs},
        vote_sender_types::ReplayVoteSender,
    };
    use solana_sdk::{
        hash::Hash,
        signature::{Keypair, Signature, Signer},
    };
    use solana_vote_program::vote_state::Vote;
    use std::collections::BTreeSet;

    #[test]
    fn test_max_vote_tx_fits() {
        solana_logger::setup();
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let slots: Vec<_> = (0..31).collect();

        let vote_tx = vote_transaction::new_vote_transaction(
            slots,
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &vote_keypair,
            Some(Hash::default()),
        );

        use bincode::serialized_size;
        info!("max vote size {}", serialized_size(&vote_tx).unwrap());

        let msgs = packet::to_packets(&[vote_tx]); // panics if won't fit

        assert_eq!(msgs.len(), 1);
    }

    fn run_vote_contains_authorized_voter(hash: Option<Hash>) {
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let authorized_voter = Keypair::new();

        let vote_tx = vote_transaction::new_vote_transaction(
            vec![0],
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &authorized_voter,
            hash,
        );

        // Check that the two signing keys pass the check
        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &node_keypair.pubkey()
        ));

        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &authorized_voter.pubkey()
        ));

        // Non signing key shouldn't pass the check
        assert!(!VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &vote_keypair.pubkey()
        ));

        // Set the authorized voter == vote keypair
        let vote_tx = vote_transaction::new_vote_transaction(
            vec![0],
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &vote_keypair,
            hash,
        );

        // Check that the node_keypair and vote keypair pass the authorized voter check
        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &node_keypair.pubkey()
        ));

        assert!(VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &vote_keypair.pubkey()
        ));

        // The other keypair should not pass the check
        assert!(!VoteTracker::vote_contains_authorized_voter(
            &vote_tx,
            &authorized_voter.pubkey()
        ));
    }

    #[test]
    fn test_vote_contains_authorized_voter() {
        run_vote_contains_authorized_voter(None);
        run_vote_contains_authorized_voter(Some(Hash::default()));
    }

    #[test]
    fn test_update_new_root() {
        let (vote_tracker, bank, _, _) = setup();

        // Check outdated slots are purged with new root
        let new_voter = Arc::new(solana_sdk::pubkey::new_rand());
        // Make separate copy so the original doesn't count toward
        // the ref count, which would prevent cleanup
        let new_voter_ = Arc::new(*new_voter);
        vote_tracker.insert_vote(bank.slot(), new_voter_);
        assert!(vote_tracker
            .slot_vote_trackers
            .read()
            .unwrap()
            .contains_key(&bank.slot()));
        let bank1 = Bank::new_from_parent(&bank, &Pubkey::default(), bank.slot() + 1);
        vote_tracker.progress_with_new_root_bank(&bank1);
        assert!(!vote_tracker
            .slot_vote_trackers
            .read()
            .unwrap()
            .contains_key(&bank.slot()));

        // Check `keys` and `epoch_authorized_voters` are purged when new
        // root bank moves to the next epoch
        assert!(vote_tracker.keys.0.read().unwrap().contains(&new_voter));
        let current_epoch = bank.epoch();
        let new_epoch_bank = Bank::new_from_parent(
            &bank,
            &Pubkey::default(),
            bank.epoch_schedule()
                .get_first_slot_in_epoch(current_epoch + 1),
        );
        vote_tracker.progress_with_new_root_bank(&new_epoch_bank);
        assert!(!vote_tracker.keys.0.read().unwrap().contains(&new_voter));
        assert_eq!(
            *vote_tracker.current_epoch.read().unwrap(),
            current_epoch + 1
        );
    }

    #[test]
    fn test_update_new_leader_schedule_epoch() {
        let (vote_tracker, bank, _, _) = setup();

        // Check outdated slots are purged with new root
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let next_leader_schedule_epoch = leader_schedule_epoch + 1;
        let mut next_leader_schedule_computed = bank.slot();
        loop {
            next_leader_schedule_computed += 1;
            if bank.get_leader_schedule_epoch(next_leader_schedule_computed)
                == next_leader_schedule_epoch
            {
                break;
            }
        }
        assert_eq!(
            bank.get_leader_schedule_epoch(next_leader_schedule_computed),
            next_leader_schedule_epoch
        );
        let next_leader_schedule_bank =
            Bank::new_from_parent(&bank, &Pubkey::default(), next_leader_schedule_computed);
        vote_tracker.progress_leader_schedule_epoch(&next_leader_schedule_bank);
        assert_eq!(
            *vote_tracker.leader_schedule_epoch.read().unwrap(),
            next_leader_schedule_epoch
        );
        assert_eq!(
            vote_tracker
                .epoch_authorized_voters
                .read()
                .unwrap()
                .get(&next_leader_schedule_epoch)
                .unwrap(),
            next_leader_schedule_bank
                .epoch_stakes(next_leader_schedule_epoch)
                .unwrap()
                .epoch_authorized_voters()
        );
    }

    #[test]
    fn test_votes_in_range() {
        // Create some voters at genesis
        let stake_per_validator = 100;
        let (vote_tracker, _, validator_voting_keypairs, subscriptions) = setup();
        let (votes_sender, votes_receiver) = unbounded();
        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );

        let bank0 = Bank::new(&genesis_config);
        // Votes for slots less than the provided root bank's slot should not be processed
        let bank3 = Arc::new(Bank::new_from_parent(
            &Arc::new(bank0),
            &Pubkey::default(),
            3,
        ));
        let vote_slots = vec![1, 2];
        send_vote_txs(
            vote_slots,
            vec![],
            &validator_voting_keypairs,
            None,
            &votes_sender,
            &replay_votes_sender,
        );
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_receiver,
            &vote_tracker,
            &bank3,
            &subscriptions,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
        )
        .unwrap();

        // Vote slots for slots greater than root bank's set of currently calculated epochs
        // are ignored
        let max_epoch = bank3.get_leader_schedule_epoch(bank3.slot());
        assert!(bank3.epoch_stakes(max_epoch).is_some());
        let unknown_epoch = max_epoch + 1;
        assert!(bank3.epoch_stakes(unknown_epoch).is_none());
        let first_slot_in_unknown_epoch = bank3
            .epoch_schedule()
            .get_first_slot_in_epoch(unknown_epoch);
        let vote_slots = vec![first_slot_in_unknown_epoch, first_slot_in_unknown_epoch + 1];
        send_vote_txs(
            vote_slots,
            vec![],
            &validator_voting_keypairs,
            None,
            &votes_sender,
            &replay_votes_sender,
        );
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_receiver,
            &vote_tracker,
            &bank3,
            &subscriptions,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
        )
        .unwrap();

        // Should be no updates since everything was ignored
        assert!(vote_tracker.slot_vote_trackers.read().unwrap().is_empty());
    }

    fn send_vote_txs(
        gossip_vote_slots: Vec<Slot>,
        replay_vote_slots: Vec<Slot>,
        validator_voting_keypairs: &[ValidatorVoteKeypairs],
        switch_proof_hash: Option<Hash>,
        votes_sender: &VerifiedVoteTransactionsSender,
        replay_votes_sender: &ReplayVoteSender,
    ) {
        validator_voting_keypairs.iter().for_each(|keypairs| {
            let node_keypair = &keypairs.node_keypair;
            let vote_keypair = &keypairs.vote_keypair;
            let vote_tx = vote_transaction::new_vote_transaction(
                gossip_vote_slots.clone(),
                Hash::default(),
                Hash::default(),
                node_keypair,
                vote_keypair,
                vote_keypair,
                switch_proof_hash,
            );
            votes_sender.send(vec![vote_tx]).unwrap();
            let replay_vote = Vote::new(replay_vote_slots.clone(), Hash::default());
            // Send same vote twice, but should only notify once
            for _ in 0..2 {
                replay_votes_sender
                    .send((
                        vote_keypair.pubkey(),
                        replay_vote.clone(),
                        switch_proof_hash,
                    ))
                    .unwrap();
            }
        });
    }

    fn run_test_process_votes(hash: Option<Hash>) {
        // Create some voters at genesis
        let stake_per_validator = 100;
        let (vote_tracker, _, validator_voting_keypairs, subscriptions) = setup();
        let (votes_txs_sender, votes_txs_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );
        let bank0 = Bank::new(&genesis_config);

        let gossip_vote_slots = vec![1, 2];
        let replay_vote_slots = vec![3, 4];
        send_vote_txs(
            gossip_vote_slots.clone(),
            replay_vote_slots.clone(),
            &validator_voting_keypairs,
            hash,
            &votes_txs_sender,
            &replay_votes_sender,
        );

        // Check that all the votes were registered for each validator correctly
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_txs_receiver,
            &vote_tracker,
            &bank0,
            &subscriptions,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
        )
        .unwrap();

        // Check that the received votes were pushed to other commponents
        // subscribing via `verified_vote_receiver`
        let all_expected_slots: BTreeSet<_> = gossip_vote_slots
            .into_iter()
            .chain(replay_vote_slots.into_iter())
            .collect();
        let mut pubkey_to_votes: HashMap<Pubkey, BTreeSet<Slot>> = HashMap::new();
        for (received_pubkey, new_votes) in verified_vote_receiver.try_iter() {
            let already_received_votes = pubkey_to_votes.entry(received_pubkey).or_default();
            for new_vote in new_votes {
                // `new_vote` should only be received once
                assert!(already_received_votes.insert(new_vote));
            }
        }
        assert_eq!(pubkey_to_votes.len(), validator_voting_keypairs.len());
        for keypairs in &validator_voting_keypairs {
            assert_eq!(
                *pubkey_to_votes
                    .get(&keypairs.vote_keypair.pubkey())
                    .unwrap(),
                all_expected_slots
            );
        }

        // Check the vote trackers were updated correctly
        for vote_slot in all_expected_slots {
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(vote_slot).unwrap();
            let r_slot_vote_tracker = slot_vote_tracker.read().unwrap();
            for voting_keypairs in &validator_voting_keypairs {
                let pubkey = voting_keypairs.vote_keypair.pubkey();
                assert!(r_slot_vote_tracker.voted.contains_key(&pubkey));
                assert!(r_slot_vote_tracker
                    .updates
                    .as_ref()
                    .unwrap()
                    .contains(&Arc::new(pubkey)));
                // Only the last vote in the stack of `gossip_votes` should count towards
                // the `optimistic` vote set.
                let optimistic_votes_tracker =
                    r_slot_vote_tracker.optimistic_votes_tracker(&Hash::default());
                if vote_slot == 2 || vote_slot == 4 {
                    let optimistic_votes_tracker = optimistic_votes_tracker.unwrap();
                    assert!(optimistic_votes_tracker.voted().contains(&pubkey));
                    assert_eq!(
                        optimistic_votes_tracker.stake(),
                        stake_per_validator * validator_voting_keypairs.len() as u64
                    );
                } else {
                    assert!(optimistic_votes_tracker.is_none())
                }
            }
        }
    }

    #[test]
    fn test_process_votes1() {
        run_test_process_votes(None);
        run_test_process_votes(Some(Hash::default()));
    }

    #[test]
    fn test_process_votes2() {
        // Create some voters at genesis
        let (vote_tracker, _, validator_voting_keypairs, subscriptions) = setup();

        // Create bank with the voters
        let stake_per_validator = 100;
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );
        let bank0 = Bank::new(&genesis_config);

        // Send some votes to process
        let (votes_txs_sender, votes_txs_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();
        let (_replay_votes_sender, replay_votes_receiver) = unbounded();

        let mut expected_votes = vec![];
        let num_voters_per_slot = 2;
        let bank_hash = Hash::default();
        for (i, keyset) in validator_voting_keypairs
            .chunks(num_voters_per_slot)
            .enumerate()
        {
            let validator_votes: Vec<_> = keyset
                .iter()
                .map(|keypairs| {
                    let node_keypair = &keypairs.node_keypair;
                    let vote_keypair = &keypairs.vote_keypair;
                    expected_votes.push((vote_keypair.pubkey(), vec![i as Slot + 1]));
                    vote_transaction::new_vote_transaction(
                        vec![i as u64 + 1],
                        bank_hash,
                        Hash::default(),
                        node_keypair,
                        vote_keypair,
                        vote_keypair,
                        None,
                    )
                })
                .collect();
            votes_txs_sender.send(validator_votes).unwrap();
        }

        // Read and process votes from channel `votes_receiver`
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_txs_receiver,
            &vote_tracker,
            &bank0,
            &subscriptions,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
        )
        .unwrap();

        // Check that the received votes were pushed to other commponents
        // subscribing via a channel
        let received_votes: Vec<_> = verified_vote_receiver.try_iter().collect();
        assert_eq!(received_votes.len(), validator_voting_keypairs.len());
        for (expected_pubkey_vote, received_pubkey_vote) in
            expected_votes.iter().zip(received_votes.iter())
        {
            assert_eq!(expected_pubkey_vote, received_pubkey_vote);
        }

        // Check that all the votes were registered for each validator correctly
        for (i, keyset) in validator_voting_keypairs.chunks(2).enumerate() {
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(i as u64 + 1).unwrap();
            let r_slot_vote_tracker = &slot_vote_tracker.read().unwrap();
            for voting_keypairs in keyset {
                let pubkey = voting_keypairs.vote_keypair.pubkey();
                assert!(r_slot_vote_tracker.voted.contains_key(&pubkey));
                assert!(r_slot_vote_tracker
                    .updates
                    .as_ref()
                    .unwrap()
                    .contains(&Arc::new(pubkey)));
                // All the votes were single votes, so they should all count towards
                // the optimistic confirmation vote set
                let optimistic_votes_tracker = r_slot_vote_tracker
                    .optimistic_votes_tracker(&bank_hash)
                    .unwrap();
                assert!(optimistic_votes_tracker.voted().contains(&pubkey));
                assert_eq!(
                    optimistic_votes_tracker.stake(),
                    num_voters_per_slot as u64 * stake_per_validator
                );
            }
        }
    }

    fn run_test_process_votes3(switch_proof_hash: Option<Hash>) {
        let (votes_sender, votes_receiver) = unbounded();
        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();

        let vote_slot = 1;
        let vote_bank_hash = Hash::default();
        // Events:
        // 0: Send gossip vote
        // 1: Send replay vote
        // 2: Send both
        let ordered_events = vec![
            vec![0],
            vec![1],
            vec![0, 1],
            vec![1, 0],
            vec![2],
            vec![0, 1, 2],
            vec![1, 0, 2],
            vec![0, 1, 2, 0, 1, 2],
        ];
        for events in ordered_events {
            let (vote_tracker, bank, validator_voting_keypairs, subscriptions) = setup();
            let node_keypair = &validator_voting_keypairs[0].node_keypair;
            let vote_keypair = &validator_voting_keypairs[0].vote_keypair;
            for &e in &events {
                if e == 0 || e == 2 {
                    // Create vote transaction
                    let vote_tx = vote_transaction::new_vote_transaction(
                        vec![vote_slot],
                        vote_bank_hash,
                        Hash::default(),
                        node_keypair,
                        vote_keypair,
                        vote_keypair,
                        switch_proof_hash,
                    );
                    votes_sender.send(vec![vote_tx.clone()]).unwrap();
                }
                if e == 1 || e == 2 {
                    replay_votes_sender
                        .send((
                            vote_keypair.pubkey(),
                            Vote::new(vec![vote_slot], Hash::default()),
                            switch_proof_hash,
                        ))
                        .unwrap();
                }
                let _ = ClusterInfoVoteListener::listen_and_confirm_votes(
                    &votes_receiver,
                    &vote_tracker,
                    &bank,
                    &subscriptions,
                    &verified_vote_sender,
                    &replay_votes_receiver,
                    &None,
                );
            }
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(vote_slot).unwrap();
            let r_slot_vote_tracker = &slot_vote_tracker.read().unwrap();

            if events == vec![1] {
                // Check `gossip_only_stake` is not incremented
                assert_eq!(
                    r_slot_vote_tracker
                        .optimistic_votes_tracker(&vote_bank_hash)
                        .unwrap()
                        .stake(),
                    100
                );
                assert_eq!(r_slot_vote_tracker.gossip_only_stake, 0);
            } else {
                // Check that both the `gossip_only_stake` and `total_voted_stake` both
                // increased
                assert_eq!(
                    r_slot_vote_tracker
                        .optimistic_votes_tracker(&vote_bank_hash)
                        .unwrap()
                        .stake(),
                    100
                );
                assert_eq!(r_slot_vote_tracker.gossip_only_stake, 100);
            }
        }
    }

    #[test]
    fn test_run_test_process_votes3() {
        run_test_process_votes3(None);
        run_test_process_votes3(Some(Hash::default()));
    }

    #[test]
    fn test_get_voters_by_epoch() {
        // Create some voters at genesis
        let (vote_tracker, bank, validator_voting_keypairs, _) = setup();
        let last_known_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let last_known_slot = bank
            .epoch_schedule()
            .get_last_slot_in_epoch(last_known_epoch);

        // Check we can get the authorized voters
        for keypairs in &validator_voting_keypairs {
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), last_known_slot)
                .is_some());
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), last_known_slot + 1)
                .is_none());
        }

        // Create the set of relevant voters for the next epoch
        let new_epoch = last_known_epoch + 1;
        let first_slot_in_new_epoch = bank.epoch_schedule().get_first_slot_in_epoch(new_epoch);
        let new_keypairs: Vec<_> = (0..10).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let new_epoch_authorized_voters: HashMap<_, _> = new_keypairs
            .iter()
            .chain(validator_voting_keypairs[0..5].iter())
            .map(|keypair| (keypair.vote_keypair.pubkey(), keypair.vote_keypair.pubkey()))
            .collect();

        vote_tracker
            .epoch_authorized_voters
            .write()
            .unwrap()
            .insert(new_epoch, Arc::new(new_epoch_authorized_voters));

        // These keypairs made it into the new epoch
        for keypairs in new_keypairs
            .iter()
            .chain(validator_voting_keypairs[0..5].iter())
        {
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), first_slot_in_new_epoch)
                .is_some());
        }

        // These keypairs were not refreshed in new epoch
        for keypairs in validator_voting_keypairs[5..10].iter() {
            assert!(vote_tracker
                .get_authorized_voter(&keypairs.vote_keypair.pubkey(), first_slot_in_new_epoch)
                .is_none());
        }
    }

    #[test]
    fn test_vote_tracker_references() {
        // The number of references that get stored for a pubkey every time
        // a vote is added to the tracking set via a transaction. One stored in the
        // SlotVoteTracker.voted, one in SlotVoteTracker.updates, one in
        // SlotVoteTracker.optimistic_votes_tracker
        let ref_count_per_vote = 3;
        let ref_count_per_new_key = 1;

        // Create some voters at genesis
        let validator_keypairs: Vec<_> =
            (0..2).map(|_| ValidatorVoteKeypairs::new_rand()).collect();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_keypairs,
                vec![100; validator_keypairs.len()],
            );
        let bank = Bank::new(&genesis_config);
        let exit = Arc::new(AtomicBool::new(false));
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank = bank_forks.read().unwrap().get(0).unwrap().clone();
        let vote_tracker = VoteTracker::new(&bank);
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));

        // Send a vote to process, should add a reference to the pubkey for that voter
        // in the tracker
        let validator0_keypairs = &validator_keypairs[0];
        let voted_slot = bank.slot() + 1;
        let vote_tx = vec![vote_transaction::new_vote_transaction(
            // Must vote > root to be processed
            vec![voted_slot],
            Hash::default(),
            Hash::default(),
            &validator0_keypairs.node_keypair,
            &validator0_keypairs.vote_keypair,
            &validator0_keypairs.vote_keypair,
            None,
        )];

        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vote_tx,
            // Add gossip vote for same slot, should not affect outcome
            vec![(
                validator0_keypairs.vote_keypair.pubkey(),
                Vote::new(vec![voted_slot], Hash::default()),
                None,
            )],
            &bank,
            &subscriptions,
            &verified_vote_sender,
            &None,
        );
        let ref_count = Arc::strong_count(
            &vote_tracker
                .keys
                .0
                .read()
                .unwrap()
                .get(&validator0_keypairs.vote_keypair.pubkey())
                .unwrap(),
        );

        // This new pubkey submitted a vote for a slot, so ref count is
        // `ref_count_per_vote + ref_count_per_new_key`.
        // +ref_count_per_new_key for the new pubkey  in `vote_tracker.keys` and
        // +ref_count_per_vote for the one new vote
        let mut current_ref_count = ref_count_per_vote + ref_count_per_new_key;
        assert_eq!(ref_count, current_ref_count);

        // Setup next epoch
        let old_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let new_epoch = old_epoch + 1;
        let new_epoch_vote_accounts: HashMap<_, _> = vec![(
            validator0_keypairs.vote_keypair.pubkey(),
            validator0_keypairs.vote_keypair.pubkey(),
        )]
        .into_iter()
        .collect();
        vote_tracker
            .epoch_authorized_voters
            .write()
            .unwrap()
            .insert(new_epoch, Arc::new(new_epoch_vote_accounts));

        // Test with votes across two epochs
        let first_slot_in_new_epoch = bank.epoch_schedule().get_first_slot_in_epoch(new_epoch);

        // Make 2 new votes in two different epochs for the same pubkey,
        // the ref count should go up by 3 * ref_count_per_vote
        // Add 1 vote through the replay channel for a different pubkey,
        // ref count should equal `current_ref_count` for that pubkey.
        let vote_txs: Vec<_> = [first_slot_in_new_epoch - 1, first_slot_in_new_epoch]
            .iter()
            .map(|slot| {
                vote_transaction::new_vote_transaction(
                    // Must vote > root to be processed
                    vec![*slot],
                    Hash::default(),
                    Hash::default(),
                    &validator0_keypairs.node_keypair,
                    &validator0_keypairs.vote_keypair,
                    &validator0_keypairs.vote_keypair,
                    None,
                )
            })
            .collect();

        let new_root_bank =
            Bank::new_from_parent(&bank, &Pubkey::default(), first_slot_in_new_epoch - 2);
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vote_txs,
            vec![(
                validator_keypairs[1].vote_keypair.pubkey(),
                Vote::new(vec![first_slot_in_new_epoch], Hash::default()),
                None,
            )],
            &new_root_bank,
            &subscriptions,
            &verified_vote_sender,
            &None,
        );

        // Check new replay vote pubkey first
        let ref_count = Arc::strong_count(
            &vote_tracker
                .keys
                .0
                .read()
                .unwrap()
                .get(&validator_keypairs[1].vote_keypair.pubkey())
                .unwrap(),
        );
        // This new pubkey submitted a replay vote for a slot, so ref count is
        // `ref_count_per_optimistic_vote + ref_count_per_new_key`.
        // +ref_count_per_new_key for the new pubkey  in `vote_tracker.keys` and
        // +ref_count_per_optimistic_vote for the one new vote
        assert_eq!(ref_count, ref_count_per_vote + ref_count_per_new_key);

        // Check the existing pubkey
        let ref_count = Arc::strong_count(
            &vote_tracker
                .keys
                .0
                .read()
                .unwrap()
                .get(&validator0_keypairs.vote_keypair.pubkey())
                .unwrap(),
        );
        current_ref_count += 2 * ref_count_per_vote;
        assert_eq!(ref_count, current_ref_count);
    }

    fn setup() -> (
        Arc<VoteTracker>,
        Arc<Bank>,
        Vec<ValidatorVoteKeypairs>,
        Arc<RpcSubscriptions>,
    ) {
        let validator_voting_keypairs: Vec<_> =
            (0..10).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![100; validator_voting_keypairs.len()],
            );
        let bank = Bank::new(&genesis_config);
        let vote_tracker = VoteTracker::new(&bank);
        let exit = Arc::new(AtomicBool::new(false));
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank = bank_forks.read().unwrap().get(0).unwrap().clone();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let subscriptions = Arc::new(RpcSubscriptions::new(
            &exit,
            bank_forks,
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));

        // Integrity Checks
        let current_epoch = bank.epoch();
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());

        // Check the vote tracker has all the known epoch state on construction
        for epoch in current_epoch..=leader_schedule_epoch {
            assert_eq!(
                vote_tracker
                    .epoch_authorized_voters
                    .read()
                    .unwrap()
                    .get(&epoch)
                    .unwrap(),
                bank.epoch_stakes(epoch).unwrap().epoch_authorized_voters()
            );
        }

        // Check the epoch state is correct
        assert_eq!(
            *vote_tracker.leader_schedule_epoch.read().unwrap(),
            leader_schedule_epoch,
        );
        assert_eq!(*vote_tracker.current_epoch.read().unwrap(), current_epoch);
        (
            Arc::new(vote_tracker),
            bank,
            validator_voting_keypairs,
            subscriptions,
        )
    }

    #[test]
    fn test_verify_votes_empty() {
        solana_logger::setup();
        let votes = vec![];
        let labels = vec![];
        let (vote_txs, packets) = ClusterInfoVoteListener::verify_votes(votes, labels);
        assert!(vote_txs.is_empty());
        assert!(packets.is_empty());
    }

    fn verify_packets_len(packets: &[(CrdsValueLabel, Packets)], ref_value: usize) {
        let num_packets: usize = packets.iter().map(|p| p.1.packets.len()).sum();
        assert_eq!(num_packets, ref_value);
    }

    fn test_vote_tx(hash: Option<Hash>) -> Transaction {
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let auth_voter_keypair = Keypair::new();
        vote_transaction::new_vote_transaction(
            vec![0],
            Hash::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &auth_voter_keypair,
            hash,
        )
    }

    fn run_test_verify_votes_1_pass(hash: Option<Hash>) {
        let vote_tx = test_vote_tx(hash);
        let votes = vec![vote_tx];
        let labels = vec![CrdsValueLabel::Vote(0, solana_sdk::pubkey::new_rand())];
        let (vote_txs, packets) = ClusterInfoVoteListener::verify_votes(votes, labels);
        assert_eq!(vote_txs.len(), 1);
        verify_packets_len(&packets, 1);
    }

    #[test]
    fn test_verify_votes_1_pass() {
        run_test_verify_votes_1_pass(None);
        run_test_verify_votes_1_pass(Some(Hash::default()));
    }

    fn run_test_bad_vote(hash: Option<Hash>) {
        let vote_tx = test_vote_tx(hash);
        let mut bad_vote = vote_tx.clone();
        bad_vote.signatures[0] = Signature::default();
        let votes = vec![vote_tx.clone(), bad_vote, vote_tx];
        let label = CrdsValueLabel::Vote(0, solana_sdk::pubkey::new_rand());
        let labels: Vec<_> = (0..votes.len()).map(|_| label.clone()).collect();
        let (vote_txs, packets) = ClusterInfoVoteListener::verify_votes(votes, labels);
        assert_eq!(vote_txs.len(), 2);
        verify_packets_len(&packets, 2);
    }

    #[test]
    fn test_sum_stake() {
        let (_, bank, validator_voting_keypairs, _) = setup();
        let vote_keypair = &validator_voting_keypairs[0].vote_keypair;
        let epoch_stakes = bank.epoch_stakes(bank.epoch()).unwrap();
        let mut gossip_only_stake = 0;

        ClusterInfoVoteListener::sum_stake(
            &mut gossip_only_stake,
            Some(epoch_stakes),
            &vote_keypair.pubkey(),
        );
        assert_eq!(gossip_only_stake, 100);
    }

    #[test]
    fn test_bad_vote() {
        run_test_bad_vote(None);
        run_test_bad_vote(Some(Hash::default()));
    }
}
