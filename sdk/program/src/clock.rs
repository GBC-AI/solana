//! Provides information about the network's clock which is made up of ticks, slots, etc...

toml_config::package_config! {
    DEFAULT_TICKS_PER_SECOND: u64,
    DEFAULT_TICKS_PER_SLOT: u64,
    DEFAULT_HASHES_PER_SECOND: u64,
    DEFAULT_DEV_SLOTS_PER_EPOCH: u64,

    NUM_CONSECUTIVE_LEADER_SLOTS: u64,
    MAX_HASH_AGE_IN_SECONDS: usize,
    MAX_TRANSACTION_FORWARDING_DELAY_GPU: usize,
    MAX_TRANSACTION_FORWARDING_DELAY: usize,
}

toml_config::derived_values! {
    MS_PER_TICK: u64 = 1000 / CFG.DEFAULT_TICKS_PER_SECOND;
    TICKS_PER_DAY: u64 = CFG.DEFAULT_TICKS_PER_SECOND * SECONDS_PER_DAY;
    DEFAULT_SLOTS_PER_EPOCH: u64 = 2 * *TICKS_PER_DAY / CFG.DEFAULT_TICKS_PER_SLOT;
    DEFAULT_MS_PER_SLOT: u64 = 1_000 * CFG.DEFAULT_TICKS_PER_SLOT / CFG.DEFAULT_TICKS_PER_SECOND;

    MAX_RECENT_BLOCKHASHES: usize =
        CFG.MAX_HASH_AGE_IN_SECONDS * CFG.DEFAULT_TICKS_PER_SECOND as usize / CFG.DEFAULT_TICKS_PER_SLOT as usize;

    MAX_PROCESSING_AGE: usize = *MAX_RECENT_BLOCKHASHES / 2;

}

pub const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Slot is a unit of time given to a leader for encoding,
///  is some some number of Ticks long.
pub type Slot = u64;

/// Epoch is a unit of time a given leader schedule is honored,
///  some number of Slots.
pub type Epoch = u64;

pub const GENESIS_EPOCH: Epoch = 0;

/// SlotIndex is an index to the slots of a epoch
pub type SlotIndex = u64;

/// SlotCount is the number of slots in a epoch
pub type SlotCount = u64;

/// UnixTimestamp is an approximate measure of real-world time,
/// expressed as Unix time (ie. seconds since the Unix epoch)
pub type UnixTimestamp = i64;

/// Clock represents network time.  Members of Clock start from 0 upon
///  network boot.  The best way to map Clock to wallclock time is to use
///  current Slot, as Epochs vary in duration (they start short and grow
///  as the network progresses).
///
#[repr(C)]
#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct Clock {
    /// the current network/bank Slot
    pub slot: Slot,
    /// unused
    pub unused: u64,
    /// the bank Epoch
    pub epoch: Epoch,
    /// the future Epoch for which the leader schedule has
    ///  most recently been calculated
    pub leader_schedule_epoch: Epoch,
    /// computed from genesis creation time and network time
    ///  in slots, drifts!
    pub unix_timestamp: UnixTimestamp,
}
