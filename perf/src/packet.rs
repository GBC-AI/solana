//! The `packet` module defines data structures and methods to pull data from the network.
use crate::{cuda_runtime::PinnedVec, recycler::Recycler};
use bincode::config::Options;
use serde::Serialize;
pub use solana_sdk::packet::{Meta, Packet, PACKET_DATA_SIZE};
use std::net::SocketAddr;

toml_config::package_config! {
    NUM_RCVMMSGS: usize,
    NUM_PACKETS: usize,
    PACKETS_PER_BATCH: usize,
}
toml_config::derived_values! {
    PACKETS_BATCH_SIZE: usize = CFG.PACKETS_PER_BATCH * PACKET_DATA_SIZE;
}

#[derive(Debug, Clone)]
pub struct Packets {
    pub packets: PinnedVec<Packet>,
}

//auto derive doesn't support large arrays
impl Default for Packets {
    fn default() -> Packets {
        let packets = PinnedVec::with_capacity(CFG.NUM_RCVMMSGS);
        Packets { packets }
    }
}

pub type PacketsRecycler = Recycler<PinnedVec<Packet>>;

impl Packets {
    pub fn new(packets: Vec<Packet>) -> Self {
        let packets = PinnedVec::from_vec(packets);
        Self { packets }
    }

    pub fn new_with_recycler(recycler: PacketsRecycler, size: usize, name: &'static str) -> Self {
        let mut packets = recycler.allocate(name);
        packets.reserve_and_pin(size);
        Packets { packets }
    }
    pub fn new_with_recycler_data(
        recycler: &PacketsRecycler,
        name: &'static str,
        mut packets: Vec<Packet>,
    ) -> Self {
        let mut vec = Self::new_with_recycler(recycler.clone(), packets.len(), name);
        vec.packets.append(&mut packets);
        vec
    }

    pub fn set_addr(&mut self, addr: &SocketAddr) {
        for m in self.packets.iter_mut() {
            m.meta.set_addr(&addr);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}

pub fn to_packets_chunked<T: Serialize>(xs: &[T], chunks: usize) -> Vec<Packets> {
    let mut out = vec![];
    for x in xs.chunks(chunks) {
        let mut p = Packets::default();
        p.packets.resize(x.len(), Packet::default());
        for (i, o) in x.iter().zip(p.packets.iter_mut()) {
            Packet::populate_packet(o, None, i).expect("serialize request");
        }
        out.push(p);
    }
    out
}

pub fn to_packets<T: Serialize>(xs: &[T]) -> Vec<Packets> {
    to_packets_chunked(xs, CFG.NUM_PACKETS)
}

pub fn to_packets_with_destination<T: Serialize>(
    recycler: PacketsRecycler,
    dests_and_data: &[(SocketAddr, T)],
) -> Packets {
    let mut out = Packets::new_with_recycler(
        recycler,
        dests_and_data.len(),
        "to_packets_with_destination",
    );
    out.packets.resize(dests_and_data.len(), Packet::default());
    for (dest_and_data, o) in dests_and_data.iter().zip(out.packets.iter_mut()) {
        if !dest_and_data.0.ip().is_unspecified() && dest_and_data.0.port() != 0 {
            if let Err(e) = Packet::populate_packet(o, Some(&dest_and_data.0), &dest_and_data.1) {
                error!("Couldn't write to packet {:?}. Data skipped.", e);
            }
        } else {
            trace!("Dropping packet, as destination is unknown");
        }
    }
    out
}

pub fn limited_deserialize<T>(data: &[u8]) -> bincode::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    bincode::options()
        .with_limit(PACKET_DATA_SIZE as u64)
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize_from(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;
    use solana_sdk::signature::{Keypair, Signer};
    use solana_sdk::system_transaction;

    #[test]
    fn test_to_packets() {
        let keypair = Keypair::new();
        let hash = Hash::new(&[1; 32]);
        let tx = system_transaction::transfer(&keypair, &keypair.pubkey(), 1, hash);
        let rv = to_packets(&[tx.clone(); 1]);
        assert_eq!(rv.len(), 1);
        assert_eq!(rv[0].packets.len(), 1);

        #[allow(clippy::useless_vec)]
        let rv = to_packets(&vec![tx.clone(); CFG.NUM_PACKETS]);
        assert_eq!(rv.len(), 1);
        assert_eq!(rv[0].packets.len(), CFG.NUM_PACKETS);

        #[allow(clippy::useless_vec)]
        let rv = to_packets(&vec![tx; CFG.NUM_PACKETS + 1]);
        assert_eq!(rv.len(), 2);
        assert_eq!(rv[0].packets.len(), CFG.NUM_PACKETS);
        assert_eq!(rv[1].packets.len(), 1);
    }

    #[test]
    fn test_to_packets_pinning() {
        let recycler = PacketsRecycler::default();
        for i in 0..2 {
            let _first_packets = Packets::new_with_recycler(recycler.clone(), i + 1, "first one");
        }
    }
}
