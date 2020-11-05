//! The `packet` module defines data structures and methods to pull data from the network.
use crate::recvmmsg::recv_mmsg;
pub use solana_perf::packet::{
    limited_deserialize, to_packets, to_packets_chunked, Packets, PacketsRecycler,
    CFG as PACKET_CFG, PACKETS_BATCH_SIZE,
};

use solana_metrics::inc_new_counter_debug;
pub use solana_sdk::packet::{Meta, Packet, PACKET_DATA_SIZE};
use std::{io::Result, net::UdpSocket, time::Instant};

pub fn recv_from(obj: &mut Packets, socket: &UdpSocket, max_wait_ms: usize) -> Result<usize> {
    let mut i = 0;
    //DOCUMENTED SIDE-EFFECT
    //Performance out of the IO without poll
    //  * block on the socket until it's readable
    //  * set the socket to non blocking
    //  * read until it fails
    //  * set it back to blocking before returning
    socket.set_nonblocking(false)?;
    trace!("receiving on {}", socket.local_addr().unwrap());
    let start = Instant::now();
    loop {
        obj.packets.resize(
            std::cmp::min(i + PACKET_CFG.NUM_RCVMMSGS, PACKET_CFG.PACKETS_PER_BATCH),
            Packet::default(),
        );
        match recv_mmsg(socket, &mut obj.packets[i..]) {
            Err(_) if i > 0 => {
                if start.elapsed().as_millis() > 1 {
                    break;
                }
            }
            Err(e) => {
                trace!("recv_from err {:?}", e);
                return Err(e);
            }
            Ok((_, npkts)) => {
                if i == 0 {
                    socket.set_nonblocking(true)?;
                }
                trace!("got {} packets", npkts);
                i += npkts;
                // Try to batch into big enough buffers
                // will cause less re-shuffling later on.
                if start.elapsed().as_millis() > max_wait_ms as u128
                    || i >= PACKET_CFG.PACKETS_PER_BATCH
                {
                    break;
                }
            }
        }
    }
    obj.packets.truncate(i);
    inc_new_counter_debug!("packets-recv_count", i);
    Ok(i)
}

pub fn send_to(obj: &Packets, socket: &UdpSocket) -> Result<()> {
    for p in &obj.packets {
        let a = p.meta.addr();
        socket.send_to(&p.data[..p.meta.size], &a)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::io::Write;
    use std::net::{SocketAddr, UdpSocket};

    #[test]
    fn test_packets_set_addr() {
        // test that the address is actually being updated
        let send_addr: SocketAddr = "127.0.0.1:123".parse().unwrap();
        let packets = vec![Packet::default()];
        let mut msgs = Packets::new(packets);
        msgs.set_addr(&send_addr);
        assert_eq!(msgs.packets[0].meta.addr(), send_addr);
    }

    #[test]
    pub fn packet_send_recv() {
        solana_logger::setup();
        let recv_socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let saddr = send_socket.local_addr().unwrap();
        let mut p = Packets::default();

        p.packets.resize(10, Packet::default());

        for m in p.packets.iter_mut() {
            m.meta.set_addr(&addr);
            m.meta.size = PACKET_DATA_SIZE;
        }
        send_to(&p, &send_socket).unwrap();

        let recvd = recv_from(&mut p, &recv_socket, 1).unwrap();

        assert_eq!(recvd, p.packets.len());

        for m in &p.packets {
            assert_eq!(m.meta.size, PACKET_DATA_SIZE);
            assert_eq!(m.meta.addr(), saddr);
        }
    }

    #[test]
    pub fn debug_trait() {
        write!(io::sink(), "{:?}", Packet::default()).unwrap();
        write!(io::sink(), "{:?}", Packets::default()).unwrap();
    }

    #[test]
    fn test_packet_partial_eq() {
        let mut p1 = Packet::default();
        let mut p2 = Packet::default();

        p1.meta.size = 1;
        p1.data[0] = 0;

        p2.meta.size = 1;
        p2.data[0] = 0;

        assert!(p1 == p2);

        p2.data[0] = 4;
        assert!(p1 != p2);
    }

    #[test]
    fn test_packet_resize() {
        solana_logger::setup();
        let recv_socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let mut p = Packets::default();
        p.packets
            .resize(PACKET_CFG.PACKETS_PER_BATCH, Packet::default());

        // Should only get PACKETS_PER_BATCH packets per iteration even
        // if a lot more were sent, and regardless of packet size
        for _ in 0..2 * PACKET_CFG.PACKETS_PER_BATCH {
            let mut p = Packets::default();
            p.packets.resize(1, Packet::default());
            for m in p.packets.iter_mut() {
                m.meta.set_addr(&addr);
                m.meta.size = 1;
            }
            send_to(&p, &send_socket).unwrap();
        }

        let recvd = recv_from(&mut p, &recv_socket, 100).unwrap();

        // Check we only got PACKETS_PER_BATCH packets
        assert_eq!(recvd, PACKET_CFG.PACKETS_PER_BATCH);
        assert_eq!(p.packets.capacity(), PACKET_CFG.PACKETS_PER_BATCH);
    }
}
