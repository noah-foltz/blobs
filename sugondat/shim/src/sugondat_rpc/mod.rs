use std::{fmt, sync::Arc};

use crate::key::Keypair;
use anyhow::Context;
use subxt::{config::Header as _, error::BlockError, rpc_params, utils::H256};
use sugondat_nmt::Namespace;
use sugondat_subxt::{
    sugondat::runtime_types::pallet_sugondat_blobs::namespace_param::UnvalidatedNamespace, Header,
};
use tokio::sync::watch;
use tracing::Level;

// NOTE: we specifically avoid prolifiration of subxt types around the codebase. To that end, we
//       avoid returning H256 and instead return [u8; 32] directly.

mod conn;

/// A high-level abstraction over a sugondat RPC client.
///
/// This client abstracts over the connection concerns and will perform automatic reconnections in
/// case of network failures. This implies that all methods will retry indefinitely until they
/// succeed.
///
/// The assumption is that the sugondat node is not malicious and generally well-behaved.
///
/// # Clone
///
/// This is a thin wrapper that can be cloned cheaply.
#[derive(Clone)]
pub struct Client {
    connector: Arc<conn::Connector>,
}

impl Client {
    /// Creates a new instance of the client. This immediately tries to connect to the sugondat
    /// node. It will *retry indefinitely* until it succeeds.
    ///
    /// The RPC URL must be a valid URL pointing to a sugondat node. If it's not a malformed URL,
    /// returns an error.
    #[tracing::instrument(level = Level::DEBUG)]
    pub async fn new(rpc_url: String, no_retry: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(
            url::Url::parse(&rpc_url).is_ok(),
            "invalid RPC URL: {}",
            rpc_url
        );

        tracing::info!("connecting to sugondat node: {}", rpc_url);
        let rpc_url = Arc::new(rpc_url);
        let me = Self {
            connector: Arc::new(conn::Connector::new(rpc_url)),
        };

        match no_retry {
            true => me.connector.try_connect().await?,
            false => me.connector.ensure_connected().await,
        };

        Ok(me)
    }

    /// Blocks until the sugondat node has finalized a block at the given height. Returns
    /// the block hash of the block at the given height.
    #[tracing::instrument(level = Level::DEBUG, skip(self))]
    pub async fn wait_finalized_height(&self, height: u64) -> [u8; 32] {
        tracing::info!("Waiting for block at height: {}", height);
        loop {
            let conn = self.connector.ensure_connected().await;
            match conn.finalized.wait_until_finalized(self, height).await {
                Some(block_hash) => return block_hash,
                None => {
                    // The watcher task has terminated. Reset the connection and retry.
                    self.connector.reset().await;
                }
            }
        }
    }

    /// Returns the block hash of the block at the given height.
    ///
    /// If there is no block at the given height, returns `None`.
    #[tracing::instrument(level = Level::DEBUG, skip(self))]
    pub async fn block_hash(&self, height: u64) -> anyhow::Result<Option<[u8; 32]>> {
        loop {
            let conn = self.connector.ensure_connected().await;
            let block_hash: Option<H256> = match conn
                .raw
                .request("chain_getBlockHash", rpc_params![height])
                .await
            {
                Ok(it) => it,
                Err(err) => {
                    tracing::error!(?err, "failed to query block hash");
                    self.connector.reset().await;
                    continue;
                }
            };

            break match block_hash {
                None => Ok(None),
                Some(h) if h == H256::zero() => {
                    // Little known fact: the sugondat node returns a zero block hash if there is no block
                    // at the given height.
                    Ok(None)
                }
                Some(block_hash) => Ok(Some(block_hash.into())),
            };
        }
    }

    /// Returns the block hash of the block at the given height,
    /// automatically retrying until it succeeds.
    /// `None` indicates the best block.
    pub async fn wait_block_hash(&self, height: u64) -> [u8; 32] {
        loop {
            match self.block_hash(height).await {
                Ok(Some(res)) => break res,
                Ok(None) => {
                    tracing::info!("Block hash not available yet");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Err(e) => {
                    // any other error is treated as a failure, resulting in the connection being reset
                    tracing::error!(?e, "failed to query block hash");
                    self.connector.reset().await;
                }
            }
        }
    }

    /// Returns the header and body of the block with the given hash.
    /// If it is not available, it returns the reason of the failure.
    /// `None` indicates the best block.
    async fn get_header_and_extrinsics(
        &self,
        block_hash: Option<[u8; 32]>,
    ) -> Result<(Header, Vec<sugondat_subxt::ExtrinsicDetails>), subxt::error::Error> {
        let block_hash = block_hash.map(H256::from);

        let conn = self.connector.ensure_connected().await;
        let res = match block_hash {
            Some(h) => conn.subxt.blocks().at(h).await,
            None => conn.subxt.blocks().at_latest().await,
        }?;

        let header = res.header();
        let body = res
            .extrinsics()
            .await?
            .iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok((header.clone(), body))
    }

    /// Returns the header and the body of the block with the given hash,
    /// automatically retrying until it succeeds.
    /// `None` indicates the best block.
    async fn wait_header_and_extrinsics(
        &self,
        block_hash: Option<[u8; 32]>,
    ) -> (Header, Vec<sugondat_subxt::ExtrinsicDetails>) {
        loop {
            match self.get_header_and_extrinsics(block_hash).await {
                Ok(res) => break res,
                Err(subxt::Error::Block(BlockError::NotFound(_hash))) => {
                    tracing::info!("Block not available yet");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Err(e) => {
                    // any other error is treated as a failure, resulting in the connection being reset
                    tracing::error!(?e, "failed to query block");
                    self.connector.reset().await;
                }
            }
        }
    }

    /// Returns the data of the block identified by the given block hash. If the block is not found
    /// returns an error.
    ///
    /// `None` indicates that the best block should be used.
    #[tracing::instrument(level = Level::DEBUG, skip(self))]
    pub async fn get_block_at(&self, block_hash: Option<[u8; 32]>) -> anyhow::Result<Block> {
        Block::from_header_and_extrinsics(self.get_header_and_extrinsics(block_hash).await?)
    }

    /// Returns the data of the block identified by the given block hash.
    /// It retries until the block is present
    ///
    /// `None` indicates that the best block should be used.
    #[tracing::instrument(level = Level::DEBUG, skip(self))]
    pub async fn wait_block_at(&self, block_hash: Option<[u8; 32]>) -> anyhow::Result<Block> {
        Block::from_header_and_extrinsics(self.wait_header_and_extrinsics(block_hash).await)
    }

    /// Submit a blob with the given namespace and signed with the given key. The block is submitted
    /// at best effort. Not much is done to ensure that the blob is actually included. If this
    /// function returned an error, that does not mean that the blob was not included.
    ///
    /// Returns a block hash in which the extrinsic was included.
    #[tracing::instrument(level = Level::DEBUG, skip(self))]
    pub async fn submit_blob(
        &self,
        blob: Vec<u8>,
        namespace: sugondat_nmt::Namespace,
        key: Keypair,
    ) -> anyhow::Result<[u8; 32]> {
        let extrinsic = sugondat_subxt::sugondat::tx()
            .blobs()
            .submit_blob(UnvalidatedNamespace(namespace.to_raw_bytes()), blob);

        let conn = self.connector.ensure_connected().await;
        let signed = conn
            .subxt
            .tx()
            .create_signed(&extrinsic, &key, Default::default())
            .await
            .with_context(|| format!("failed to validate or sign extrinsic"))?;
        let events = signed
            .submit_and_watch()
            .await
            .with_context(|| format!("failed to submit extrinsic"))?
            .wait_for_finalized_success()
            .await?;

        let block_hash = events.block_hash();
        Ok(block_hash.0)
    }
}

/// Iterates over the extrinsics in a block and extracts the timestamp.
///
/// The timestamp is considered a mandatory inherent extrinsic, therefore every block must have
/// one. An `Err` is returned if no timestamp is found.
fn extract_timestamp(extrinsics: &[sugondat_subxt::ExtrinsicDetails]) -> anyhow::Result<u64> {
    use sugondat_subxt::sugondat::timestamp::calls::types::Set as TimestampSet;
    for e in extrinsics.iter() {
        match e.as_extrinsic::<TimestampSet>() {
            Ok(Some(timestamp_extrinsic)) => return Ok(timestamp_extrinsic.now),
            _ => (),
        }
    }
    Err(anyhow::anyhow!("no timestamp found in block"))
}

/// Iterates over the extrinsics in a block and extracts the submit_blob extrinsics.
fn extract_blobs(extrinsics: Vec<sugondat_subxt::ExtrinsicDetails>) -> Vec<Blob> {
    use sugondat_subxt::sugondat::blobs::calls::types::SubmitBlob;

    let mut blobs = vec![];
    for (extrinsic_index, e) in extrinsics.iter().enumerate() {
        let Some(sender) = e
            .address_bytes()
            .filter(|a| a.len() == 33)
            .and_then(|a| a[1..].try_into().ok())
        else {
            continue;
        };
        let Ok(Some(SubmitBlob { namespace_id, blob })) = e.as_extrinsic::<SubmitBlob>() else {
            // Not a submit blob extrinsic, skip.
            continue;
        };
        // we are scanning the extrinsics of a block, presumably, received from an RPC from a node.
        // The block should be from a correct chain. The STF, atm, is defined in such a way, that
        // it bork the block if the namespace is invalid.
        //
        // However, there is no absolute guarantee of it, because the RPC could send anything really
        // or the shim could be outdated. So we don't panic here.
        let UnvalidatedNamespace(namespace_id) = namespace_id;
        let namespace = sugondat_nmt::Namespace::from_raw_bytes(namespace_id);
        blobs.push(Blob {
            extrinsic_index: extrinsic_index as u32,
            namespace,
            sender,
            data: blob,
        })
    }
    blobs
}

/// Examines the header and extracts the tree root committed as one of the logs.
///
/// Returns None if no tree root was found or if the tree root was malformed.
fn tree_root(header: &Header) -> Option<sugondat_nmt::TreeRoot> {
    use subxt::config::substrate::DigestItem;
    let nmt_digest_bytes = header.digest.logs.iter().find_map(|log| match log {
        DigestItem::Other(ref bytes) if bytes.starts_with(b"snmt") => Some(&bytes[4..]),
        _ => None,
    })?;
    let nmt_root: [u8; 68] = nmt_digest_bytes.try_into().ok()?;
    Some(sugondat_nmt::TreeRoot::from_raw_bytes(&nmt_root))
}

/// A small gadget that watches the finalized block headers and remembers the last one.
struct FinalizedHeadWatcher {
    /// The last finalized block header watch value.
    ///
    /// Initialized with 0 as a dummy value.
    rx: watch::Receiver<(u64, [u8; 32])>,
    /// The join handle of the task that watches the finalized block headers.
    handle: tokio::task::JoinHandle<()>,
}

impl FinalizedHeadWatcher {
    /// Spawns the watch task.
    async fn spawn(subxt: sugondat_subxt::Client) -> Self {
        let (tx, rx) = watch::channel((0, [0; 32]));
        let handle = tokio::spawn({
            async move {
                // In case of an error, the subxt client becomes unusable. The task will be
                // terminated in case of an error.
                let Ok(mut stream) = subxt.backend().stream_finalized_block_headers().await else {
                    return;
                };
                while let Some(header) = stream.next().await {
                    let Ok((header, block_ref)) = header else {
                        return;
                    };
                    let _ = tx.send((header.number as u64, block_ref.hash().0));
                }
            }
        });
        Self { rx, handle }
    }

    /// Wait until the sugondat node has finalized a block at the given height. Returns the block
    /// hash of that finalized block, or `None` in case the watcher task has terminated.
    async fn wait_until_finalized(&self, client: &Client, height: u64) -> Option<[u8; 32]> {
        let mut rx = self.rx.clone();
        let (finalized_height, block_hash) = loop {
            if let Err(_) = rx.changed().await {
                // The sender half was dropped means the watcher task has terminated.
                return None;
            }
            let (finalized_height, block_hash) = *rx.borrow();
            if finalized_height < height {
                continue;
            }
            break (finalized_height, block_hash);
        };
        if finalized_height == height {
            // The common case: the finalized block is at the height we're looking for.
            Some(block_hash)
        } else {
            // The finalized block is already past the height we're looking for, but we need to
            // return the block hash of the block at the given height. Therefore, we query it.
            loop {
                let Ok(Some(block_hash)) = client.block_hash(height).await else {
                    // TODO: throttle the retries. // TODO: return an error
                    continue;
                };
                break Some(block_hash);
            }
        }
    }
}

impl Drop for FinalizedHeadWatcher {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

mod err {
    pub fn no_tree_root() -> anyhow::Error {
        anyhow::anyhow!("no tree root found in block header. Are you sure this is a sugondat node?")
    }
}

/// Represents a sugondat block.
pub struct Block {
    pub number: u64,
    pub hash: [u8; 32],
    pub parent_hash: [u8; 32],
    pub tree_root: sugondat_nmt::TreeRoot,
    pub timestamp: u64,
    pub blobs: Vec<Blob>,
}

impl Block {
    fn from_header_and_extrinsics(
        value: (Header, Vec<sugondat_subxt::ExtrinsicDetails>),
    ) -> anyhow::Result<Self> {
        let (header, extrinsics) = value;
        let tree_root = tree_root(&header).ok_or_else(err::no_tree_root)?;
        let timestamp = extract_timestamp(&extrinsics)?;
        let blobs = extract_blobs(extrinsics);
        tracing::debug!(?blobs, "found {} blobs in block", blobs.len());
        Ok(Block {
            number: header.number as u64,
            hash: header.hash().0,
            parent_hash: header.parent_hash.0,
            tree_root,
            timestamp,
            blobs,
        })
    }
}

/// Represents a blob in a sugondat block.
pub struct Blob {
    pub extrinsic_index: u32,
    pub namespace: Namespace,
    pub sender: [u8; 32],
    pub data: Vec<u8>,
}

impl Blob {
    pub fn sha2_hash(&self) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(&self.data).into()
    }
}

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let abridged_data = if self.data.len() > 16 {
            let mut s = hex::encode(&self.data[..16]);
            s.push_str("...");
            s
        } else {
            hex::encode(&self.data)
        };
        f.debug_struct("Blob")
            .field("extrinsic_index", &self.extrinsic_index)
            .field("namespace", &self.namespace)
            .field("sender", &self.sender)
            .field("data", &abridged_data)
            .finish()
    }
}
