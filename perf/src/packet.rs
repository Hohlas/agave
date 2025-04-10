//! The `packet` module defines data structures and methods to pull data from the network.
pub use solana_packet::{self, Meta, Packet, PacketFlags, PACKET_DATA_SIZE};
use {
    crate::{cuda_runtime::PinnedVec, recycler::Recycler},
    bincode::config::Options,
    rayon::prelude::{IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator},
    serde::{de::DeserializeOwned, Deserialize, Serialize},
    std::{
        borrow::Borrow,
        io::Read,
        net::SocketAddr,
        ops::{Deref, DerefMut, Index, IndexMut},
        slice::{Iter, SliceIndex},
    },
};

pub const NUM_PACKETS: usize = 1024 * 8;

pub const PACKETS_PER_BATCH: usize = 64;
pub const NUM_RCVMMSGS: usize = 64;

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PacketBatch {
    packets: PinnedVec<Packet>,
}

pub type PacketBatchRecycler = Recycler<PinnedVec<Packet>>;

impl PacketBatch {
    pub fn new(packets: Vec<Packet>) -> Self {
        let packets = PinnedVec::from_vec(packets);
        Self { packets }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let packets = PinnedVec::with_capacity(capacity);
        Self { packets }
    }

    pub fn new_pinned_with_capacity(capacity: usize) -> Self {
        let mut batch = Self::with_capacity(capacity);
        batch.packets.reserve_and_pin(capacity);
        batch
    }

    pub fn new_unpinned_with_recycler(
        recycler: &PacketBatchRecycler,
        capacity: usize,
        name: &'static str,
    ) -> Self {
        let mut packets = recycler.allocate(name);
        packets.reserve(capacity);
        Self { packets }
    }

    pub fn new_with_recycler(
        recycler: &PacketBatchRecycler,
        capacity: usize,
        name: &'static str,
    ) -> Self {
        let mut packets = recycler.allocate(name);
        packets.reserve_and_pin(capacity);
        Self { packets }
    }

    pub fn new_with_recycler_data(
        recycler: &PacketBatchRecycler,
        name: &'static str,
        mut packets: Vec<Packet>,
    ) -> Self {
        let mut batch = Self::new_with_recycler(recycler, packets.len(), name);
        batch.packets.append(&mut packets);
        batch
    }

    pub fn new_unpinned_with_recycler_data_and_dests<S, T>(
        recycler: &PacketBatchRecycler,
        name: &'static str,
        dests_and_data: impl IntoIterator<Item = (S, T), IntoIter: ExactSizeIterator>,
    ) -> Self
    where
        S: Borrow<SocketAddr>,
        T: solana_packet::Encode,
    {
        let dests_and_data = dests_and_data.into_iter();
        let mut batch = Self::new_unpinned_with_recycler(recycler, dests_and_data.len(), name);
        batch
            .packets
            .resize(dests_and_data.len(), Packet::default());

        for ((addr, data), packet) in dests_and_data.zip(batch.packets.iter_mut()) {
            let addr = addr.borrow();
            if !addr.ip().is_unspecified() && addr.port() != 0 {
                if let Err(e) = Packet::populate_packet(packet, Some(addr), &data) {
                    // TODO: This should never happen. Instead the caller should
                    // break the payload into smaller messages, and here any errors
                    // should be propagated.
                    error!("Couldn't write to packet {:?}. Data skipped.", e);
                    packet.meta_mut().set_discard(true);
                }
            } else {
                trace!("Dropping packet, as destination is unknown");
                packet.meta_mut().set_discard(true);
            }
        }
        batch
    }

    pub fn new_unpinned_with_recycler_data(
        recycler: &PacketBatchRecycler,
        name: &'static str,
        mut packets: Vec<Packet>,
    ) -> Self {
        let mut batch = Self::new_unpinned_with_recycler(recycler, packets.len(), name);
        batch.packets.append(&mut packets);
        batch
    }

    pub fn set_addr(&mut self, addr: &SocketAddr) {
        for p in self.iter_mut() {
            p.meta_mut().set_socket_addr(addr);
        }
    }
}

impl Deref for PacketBatch {
    type Target = PinnedVec<Packet>;

    fn deref(&self) -> &Self::Target {
        &self.packets
    }
}

impl DerefMut for PacketBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.packets
    }
}

impl<I: SliceIndex<[Packet]>> Index<I> for PacketBatch {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        &self.packets[index]
    }
}

impl<I: SliceIndex<[Packet]>> IndexMut<I> for PacketBatch {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.packets[index]
    }
}

impl<'a> IntoIterator for &'a PacketBatch {
    type Item = &'a Packet;
    type IntoIter = Iter<'a, Packet>;

    fn into_iter(self) -> Self::IntoIter {
        self.packets.iter()
    }
}

impl<'a> IntoParallelIterator for &'a PacketBatch {
    type Iter = rayon::slice::Iter<'a, Packet>;
    type Item = &'a Packet;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter()
    }
}

impl<'a> IntoParallelIterator for &'a mut PacketBatch {
    type Iter = rayon::slice::IterMut<'a, Packet>;
    type Item = &'a mut Packet;
    fn into_par_iter(self) -> Self::Iter {
        self.packets.par_iter_mut()
    }
}

impl From<PacketBatch> for Vec<Packet> {
    fn from(batch: PacketBatch) -> Self {
        batch.packets.into()
    }
}

pub fn to_packet_batches<T: Serialize>(items: &[T], chunk_size: usize) -> Vec<PacketBatch> {
    items
        .chunks(chunk_size)
        .map(|batch_items| {
            let mut batch = PacketBatch::with_capacity(batch_items.len());
            batch.resize(batch_items.len(), Packet::default());
            for (item, packet) in batch_items.iter().zip(batch.packets.iter_mut()) {
                Packet::populate_packet(packet, None, item).expect("serialize request");
            }
            batch
        })
        .collect()
}

#[cfg(test)]
fn to_packet_batches_for_tests<T: Serialize>(items: &[T]) -> Vec<PacketBatch> {
    to_packet_batches(items, NUM_PACKETS)
}

pub fn deserialize_from_with_limit<R, T>(reader: R) -> bincode::Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    // with_limit causes pre-allocation size to be limited
    // to prevent against memory exhaustion attacks.
    bincode::options()
        .with_limit(PACKET_DATA_SIZE as u64)
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize_from(reader)
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_hash::Hash, solana_keypair::Keypair, solana_signer::Signer,
        solana_system_transaction::transfer,
    };

    #[test]
    fn test_to_packet_batches() {
        let keypair = Keypair::new();
        let hash = Hash::new_from_array([1; 32]);
        let tx = transfer(&keypair, &keypair.pubkey(), 1, hash);
        let rv = to_packet_batches_for_tests(&[tx.clone(); 1]);
        assert_eq!(rv.len(), 1);
        assert_eq!(rv[0].len(), 1);

        #[allow(clippy::useless_vec)]
        let rv = to_packet_batches_for_tests(&vec![tx.clone(); NUM_PACKETS]);
        assert_eq!(rv.len(), 1);
        assert_eq!(rv[0].len(), NUM_PACKETS);

        #[allow(clippy::useless_vec)]
        let rv = to_packet_batches_for_tests(&vec![tx; NUM_PACKETS + 1]);
        assert_eq!(rv.len(), 2);
        assert_eq!(rv[0].len(), NUM_PACKETS);
        assert_eq!(rv[1].len(), 1);
    }

    #[test]
    fn test_to_packets_pinning() {
        let recycler = PacketBatchRecycler::default();
        for i in 0..2 {
            let _first_packets = PacketBatch::new_with_recycler(&recycler, i + 1, "first one");
        }
    }
}
