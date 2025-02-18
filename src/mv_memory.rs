use std::{collections::BTreeMap, sync::Mutex};

// ~x2 performance gain over a naive `RwLock<HashMap>`!
use dashmap::{
    mapref::{entry::Entry, one::Ref},
    DashMap,
};

use crate::{
    MemoryEntry, MemoryLocation, MemoryValue, ReadOrigin, ReadSet, TxIdx, TxVersion, WriteSet,
};

#[derive(Debug, Clone)]
pub(crate) enum ReadMemoryResult {
    NotFound,
    ReadError {
        blocking_tx_idx: TxIdx,
    },
    Ok {
        version: TxVersion,
        value: MemoryValue,
    },
}

// The MvMemory contains shared memory in a form of a multi-version data
// structure for values written and read by different transactions. It stores
// multiple writes for each memory location, along with a value and an associated
// version of a corresponding transaction.
// TODO: Better concurrency control if possible.
pub(crate) struct MvMemory {
    beneficiary_location: MemoryLocation,
    data: DashMap<MemoryLocation, BTreeMap<TxIdx, MemoryEntry>, ahash::RandomState>,
    last_written_locations: Vec<Mutex<Vec<MemoryLocation>>>,
    last_read_set: Vec<Mutex<ReadSet>>,
}

impl MvMemory {
    pub(crate) fn new(block_size: usize, beneficiary_location: MemoryLocation) -> Self {
        Self {
            beneficiary_location,
            data: DashMap::with_hasher(ahash::RandomState::new()),
            last_written_locations: (0..block_size).map(|_| Mutex::new(Vec::new())).collect(),
            last_read_set: (0..block_size)
                .map(|_| Mutex::new(ReadSet::new()))
                .collect(),
        }
    }

    // Apply a new pair of read & write sets to the multi-version data structure.
    // Return whether a write occurred to a memory location not written to by
    // the previous incarnation of the same transaction. This determines whether
    // the executed higher transactions need re-validation.
    pub(crate) fn record(
        &self,
        tx_version: &TxVersion,
        read_set: ReadSet,
        write_set: WriteSet,
    ) -> bool {
        *index_mutex!(self.last_read_set, tx_version.tx_idx) = read_set;

        let new_locations: Vec<MemoryLocation> = write_set.keys().cloned().collect();
        for (location, value) in write_set.into_iter() {
            let entry = MemoryEntry::Data(tx_version.tx_incarnation, value);
            // We must not use `get_mut` here, else there's a race condition where
            // two threads get `None` first, then the latter's `insert` overwrote
            // the former's.
            match self.data.entry(location) {
                Entry::Occupied(mut written_transactions) => {
                    written_transactions
                        .get_mut()
                        .insert(tx_version.tx_idx, entry);
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(BTreeMap::from([(tx_version.tx_idx, entry)]));
                }
            }
        }

        // TODO: Better error handling
        let mut last_written_locations =
            index_mutex!(self.last_written_locations, tx_version.tx_idx);

        // TODO: Faster "difference" function when there are many locations
        for prev_location in last_written_locations.iter() {
            if !new_locations.contains(prev_location) {
                if let Some(mut written_transactions) = self.data.get_mut(prev_location) {
                    written_transactions.remove(&tx_version.tx_idx);
                }
            }
        }
        for new_location in new_locations.iter() {
            if !last_written_locations.contains(new_location) {
                // We update right before returning to avoid an early clone.
                *last_written_locations = new_locations;
                return true;
            }
        }
        // We update right before returning to avoid an early clone.
        *last_written_locations = new_locations;
        false
    }

    // Obtain the last read set recorded by an execution of `tx_idx` and check
    // that re-reading each memory location in the read set still yields the
    // same value. For every value that was read, the read set stores a read
    // origin containing the version of the transaction (during the execution
    // of which the value was written), or if the value was read from storage
    // (i.e., not written by a smaller transaction). The incarnation numbers
    // are monotonically increasing, so it is sufficient to validate the read set
    // by comparing the corresponding origin.
    // This is invoked during validation, when the incarnation being validated is
    // already executed and has recorded the read set. However, if the thread
    // performing a validation for incarnation i of a transaction is slow, it is
    // possible that this function invocation observes a read set recorded by a
    // latter (> i) incarnation. In this case, incarnation i is guaranteed to be
    // already aborted (else higher incarnations would never start), and the
    // validation task will have no effect on the system regardless of the outcome
    // (only validations that successfully abort affect the state and each
    // incarnation can be aborted at most once).
    pub(crate) fn validate_read_set(&self, tx_idx: TxIdx) -> bool {
        // TODO: Better error handling
        let last_read_set = index_mutex!(self.last_read_set, tx_idx);
        for (location, prior_origin) in last_read_set.common.iter() {
            match self.read_closest(location, tx_idx) {
                ReadMemoryResult::ReadError { .. } => return false,
                ReadMemoryResult::NotFound => {
                    if *prior_origin != ReadOrigin::Storage {
                        return false;
                    }
                }
                ReadMemoryResult::Ok { version, .. } => match prior_origin {
                    ReadOrigin::Storage => return false,
                    ReadOrigin::MvMemory(v) => {
                        if *v != version {
                            return false;
                        }
                    }
                },
            }
        }
        // TODO: Fewer nests without sacrificing performance please.
        if !last_read_set.beneficiary.is_empty() {
            // TODO: Can we really skip checking for storage reads here?
            if let Some(written_beneficiary) = self.read_beneficiary() {
                for prior_origin in last_read_set.beneficiary.iter() {
                    if let ReadOrigin::MvMemory(prior_version) = prior_origin {
                        match written_beneficiary.get(&prior_version.tx_idx) {
                            Some(MemoryEntry::Data(tx_incarnation, _)) => {
                                if *tx_incarnation != prior_version.tx_incarnation {
                                    return false;
                                }
                            }
                            _ => return false,
                        }
                    }
                }
            }
        }
        true
    }

    // Replace the write set of the aborted version in the shared memory data
    // structure with special ESTIMATE markers to quickly abort higher transactions
    // that read them.
    pub(crate) fn convert_writes_to_estimates(&self, tx_idx: TxIdx) {
        // TODO: Better error handling
        for location in index_mutex!(self.last_written_locations, tx_idx).iter() {
            if let Some(mut written_transactions) = self.data.get_mut(location) {
                written_transactions.insert(tx_idx, MemoryEntry::Estimate);
            }
        }
    }

    // Find the highest transaction index among lower transactions of an input
    // transaction that has written to a memory location. This is the best guess
    // for reading speculatively, that no transaction between the highest
    // transaction found and the input transaction writes to the same memory
    // location.
    // If the entry corresponding to the highest transaction index is an ESTIMTE
    // marker, we return an error to tell the caller to postpone the execution of
    // the input transaction until the next incarnation of this highest blocking
    // index transaction completes, since it is expected to write to the ESTIMATE
    // locations again.
    // When no lower transaction has written to the memory location, a read returns
    // a not found status, implying that the value cannot be obtained from previous
    // transactions. The caller can then complete the speculative read by reading
    // from storage.
    // TODO: Refactor & make this much faster
    pub(crate) fn read_closest(
        &self,
        location: &MemoryLocation,
        tx_idx: TxIdx,
    ) -> ReadMemoryResult {
        if let Some(written_transactions) = self.data.get(location) {
            for (idx, entry) in written_transactions.iter().rev() {
                if *idx < tx_idx {
                    match entry {
                        MemoryEntry::Estimate => {
                            return ReadMemoryResult::ReadError {
                                blocking_tx_idx: *idx,
                            };
                        }
                        MemoryEntry::Data(tx_incarnation, value) => {
                            return ReadMemoryResult::Ok {
                                version: TxVersion {
                                    tx_idx: *idx,
                                    tx_incarnation: *tx_incarnation,
                                },
                                value: value.clone(),
                            }
                        }
                    }
                }
            }
        }
        ReadMemoryResult::NotFound
    }

    // For evaluating & validating explicit beneficiary reads.
    pub(crate) fn read_beneficiary(
        &self,
    ) -> Option<Ref<MemoryLocation, BTreeMap<usize, MemoryEntry>, ahash::RandomState>> {
        self.data.get(&self.beneficiary_location)
    }

    // For evaluating beneficiary at the end of the block.
    pub(crate) fn consume_beneficiary(&self) -> impl IntoIterator<Item = MemoryValue> {
        let (_, tree) = self.data.remove(&self.beneficiary_location).unwrap();
        tree.into_values().map(|entry| match entry {
            MemoryEntry::Data(_, value) => value,
            MemoryEntry::Estimate => unreachable!("Trying to consume unfinalized data!"),
        })
    }
}
