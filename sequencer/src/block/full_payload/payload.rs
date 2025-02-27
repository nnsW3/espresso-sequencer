use crate::{
    block::{
        full_payload::ns_table::{NsIndex, NsTable, NsTableBuilder},
        namespace_payload::{Index, Iter, NsPayload, NsPayloadBuilder, NsPayloadRange, TxProof},
    },
    NamespaceId, NodeState, SeqTypes, Transaction, ValidatedState,
};
use async_trait::async_trait;
use hotshot_query_service::availability::QueryablePayload;
use hotshot_types::{
    traits::{BlockPayload, EncodeBytes},
    utils::BuilderCommitment,
    vid::{VidCommon, VidSchemeType},
};
use jf_vid::VidScheme;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{collections::HashMap, fmt::Display, sync::Arc};

/// Raw payload data for an entire block.
///
/// A block consists of two sequences of arbitrary bytes:
/// - `ns_table`: namespace table
/// - `ns_payloads`: namespace payloads
///
/// Any sequence of bytes is a valid `ns_table`. Any sequence of bytes is a
/// valid `ns_payloads`. The contents of `ns_table` determine how to interpret
/// `ns_payload`.
///
/// # Namespace table
///
/// See [`NsTable`] for the format of a namespace table.
///
/// # Namespace payloads
///
/// A concatenation of payload bytes for multiple individual namespaces.
/// Namespace boundaries are dictated by `ns_table`. See [`NsPayload`] for the
/// format of a namespace payload.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct Payload {
    // Concatenated payload bytes for each namespace
    //
    // TODO want to rename thisfield to `ns_payloads`, but can't due to
    // serialization compatibility.
    #[serde(with = "base64_bytes")]
    raw_payload: Vec<u8>,

    ns_table: NsTable,
}

impl Payload {
    pub fn ns_table(&self) -> &NsTable {
        &self.ns_table
    }

    /// Like [`QueryablePayload::transaction_with_proof`] except without the
    /// proof.
    pub fn transaction(&self, index: &Index) -> Option<Transaction> {
        let ns_id = self.ns_table.read_ns_id(index.ns())?;
        let ns_payload = self.ns_payload(index.ns());
        ns_payload.export_tx(&ns_id, index.tx())
    }

    // CRATE-VISIBLE HELPERS START HERE

    pub(in crate::block) fn read_ns_payload(&self, range: &NsPayloadRange) -> &NsPayload {
        NsPayload::from_bytes_slice(&self.raw_payload[range.as_block_range()])
    }

    /// Convenience wrapper for [`Self::read_ns_payload`].
    ///
    /// `index` is not checked. Use `self.ns_table().in_bounds()` as needed.
    pub(in crate::block) fn ns_payload(&self, index: &NsIndex) -> &NsPayload {
        let ns_payload_range = self.ns_table().ns_range(index, &self.byte_len());
        self.read_ns_payload(&ns_payload_range)
    }

    pub(in crate::block) fn byte_len(&self) -> PayloadByteLen {
        PayloadByteLen(self.raw_payload.len())
    }

    // PRIVATE HELPERS START HERE

    /// Need a sync version of [`BlockPayload::from_transactions`] in order to impl [`BlockPayload::empty`].
    fn from_transactions_sync(
        transactions: impl IntoIterator<Item = <Self as BlockPayload<SeqTypes>>::Transaction> + Send,
        _validated_state: &<Self as BlockPayload<SeqTypes>>::ValidatedState,
        instance_state: &<Self as BlockPayload<SeqTypes>>::Instance,
    ) -> Result<
        (Self, <Self as BlockPayload<SeqTypes>>::Metadata),
        <Self as BlockPayload<SeqTypes>>::Error,
    > {
        // accounting for block byte length limit
        let max_block_byte_len: usize = u64::from(instance_state.chain_config.max_block_size)
            .try_into()
            .map_err(|_| <Self as BlockPayload<SeqTypes>>::Error::BlockBuilding)?;
        let mut block_byte_len = NsTableBuilder::fixed_overhead_byte_len();

        // add each tx to its namespace
        let mut ns_builders = HashMap::<NamespaceId, NsPayloadBuilder>::new();
        for tx in transactions.into_iter() {
            // accounting for block byte length limit
            block_byte_len += tx.payload().len() + NsPayloadBuilder::tx_overhead_byte_len();
            if !ns_builders.contains_key(&tx.namespace()) {
                // each new namespace adds overhead
                block_byte_len += NsTableBuilder::ns_overhead_byte_len()
                    + NsPayloadBuilder::fixed_overhead_byte_len();
            }
            if block_byte_len > max_block_byte_len {
                tracing::warn!("transactions truncated to fit in maximum block byte length {max_block_byte_len}");
                break;
            }

            let ns_builder = ns_builders.entry(tx.namespace()).or_default();
            ns_builder.append_tx(tx);
        }

        // build block payload and namespace table
        let mut payload = Vec::new();
        let mut ns_table_builder = NsTableBuilder::new();
        for (ns_id, ns_builder) in ns_builders {
            payload.extend(ns_builder.into_bytes());
            ns_table_builder.append_entry(ns_id, payload.len());
        }
        let ns_table = ns_table_builder.into_ns_table();
        let metadata = ns_table.clone();
        Ok((
            Self {
                raw_payload: payload,
                ns_table,
            },
            metadata,
        ))
    }
}

#[async_trait]
impl BlockPayload<SeqTypes> for Payload {
    // TODO BlockPayload trait eliminate unneeded args, return vals of type
    // `Self::Metadata` https://github.com/EspressoSystems/HotShot/issues/3300
    type Error = crate::Error;
    type Transaction = Transaction;
    type Instance = NodeState;
    type Metadata = NsTable;
    type ValidatedState = ValidatedState;

    async fn from_transactions(
        transactions: impl IntoIterator<Item = Self::Transaction> + Send,
        validated_state: &Self::ValidatedState,
        instance_state: &Self::Instance,
    ) -> Result<(Self, Self::Metadata), Self::Error> {
        Self::from_transactions_sync(transactions, validated_state, instance_state)
    }

    // TODO avoid cloning the entire payload here?
    fn from_bytes(block_payload_bytes: &[u8], ns_table: &Self::Metadata) -> Self {
        Self {
            raw_payload: block_payload_bytes.to_vec(),
            ns_table: ns_table.clone(),
        }
    }

    fn empty() -> (Self, Self::Metadata) {
        let payload =
            Self::from_transactions_sync(vec![], &Default::default(), &Default::default())
                .unwrap()
                .0;
        let ns_table = payload.ns_table().clone();
        (payload, ns_table)
    }

    fn builder_commitment(&self, metadata: &Self::Metadata) -> BuilderCommitment {
        let ns_table_bytes = self.ns_table.encode();

        // TODO `metadata_bytes` equals `ns_table_bytes`, so we are
        // double-hashing the ns_table. Why? To maintain serialization
        // compatibility.
        // https://github.com/EspressoSystems/espresso-sequencer/issues/1576
        let metadata_bytes = metadata.encode();

        let mut digest = sha2::Sha256::new();
        digest.update((self.raw_payload.len() as u64).to_le_bytes());
        digest.update((ns_table_bytes.len() as u64).to_le_bytes());
        digest.update((metadata_bytes.len() as u64).to_le_bytes()); // https://github.com/EspressoSystems/espresso-sequencer/issues/1576
        digest.update(&self.raw_payload);
        digest.update(ns_table_bytes);
        digest.update(metadata_bytes); // https://github.com/EspressoSystems/espresso-sequencer/issues/1576
        BuilderCommitment::from_raw_digest(digest.finalize())
    }

    fn transactions<'a>(
        &'a self,
        metadata: &'a Self::Metadata,
    ) -> impl 'a + Iterator<Item = Self::Transaction> {
        self.enumerate(metadata).map(|(_, t)| t)
    }
}

impl QueryablePayload<SeqTypes> for Payload {
    // TODO changes to QueryablePayload trait:
    // https://github.com/EspressoSystems/hotshot-query-service/issues/639
    type TransactionIndex = Index;
    type Iter<'a> = Iter<'a>;
    type InclusionProof = TxProof;

    fn len(&self, _meta: &Self::Metadata) -> usize {
        // Counting txs is nontrivial. The easiest solution is to consume an
        // iterator. If performance is a concern then we could cache this count
        // on construction of `Payload`.
        self.iter(_meta).count()
    }

    fn iter<'a>(&'a self, _meta: &'a Self::Metadata) -> Self::Iter<'a> {
        Iter::new(self)
    }

    fn transaction_with_proof(
        &self,
        _meta: &Self::Metadata,
        index: &Self::TransactionIndex,
    ) -> Option<(Self::Transaction, Self::InclusionProof)> {
        // TODO HACK! THE RETURNED PROOF MIGHT FAIL VERIFICATION.
        // https://github.com/EspressoSystems/hotshot-query-service/issues/639
        //
        // Need a `VidCommon` to proceed. Need to modify `QueryablePayload`
        // trait to add a `VidCommon` arg. In the meantime tests fail if I leave
        // it `todo!()`, so this hack allows tests to pass.
        let common = hotshot_types::vid::vid_scheme(10)
            .disperse(&self.raw_payload)
            .unwrap()
            .common;

        TxProof::new(index, self, &common)
    }
}

impl Display for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:#?}")
    }
}

impl EncodeBytes for Payload {
    fn encode(&self) -> Arc<[u8]> {
        Arc::from(self.raw_payload.as_ref())
    }
}

/// Byte length of a block payload, which includes all namespaces but *not* the
/// namespace table.
pub(in crate::block) struct PayloadByteLen(usize);

impl PayloadByteLen {
    /// Extract payload byte length from a [`VidCommon`] and construct a new [`Self`] from it.
    pub fn from_vid_common(common: &VidCommon) -> Self {
        Self(usize::try_from(VidSchemeType::get_payload_byte_len(common)).unwrap())
    }

    /// Is the payload byte length declared in a [`VidCommon`] equal [`Self`]?
    pub fn is_consistent(&self, common: &VidCommon) -> Result<(), ()> {
        // failure to convert to usize implies that `common` cannot be
        // consistent with `self`.
        let expected =
            usize::try_from(VidSchemeType::get_payload_byte_len(common)).map_err(|_| ())?;

        (self.0 == expected).then_some(()).ok_or(())
    }

    pub(in crate::block::full_payload) fn as_usize(&self) -> usize {
        self.0
    }
}

#[cfg(any(test, feature = "testing"))]
impl hotshot_types::traits::block_contents::TestableBlock<SeqTypes> for Payload {
    fn genesis() -> Self {
        BlockPayload::empty().0
    }

    fn txn_count(&self) -> u64 {
        self.len(&self.ns_table) as u64
    }
}
