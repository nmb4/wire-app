//! Restrict blob serving without restricting local reads used by chat and Docs.

use std::{
    collections::BTreeSet,
    io,
    sync::{Arc, RwLock},
};

use bytes::Bytes;
use iroh::{endpoint::Connecting, protocol::ProtocolHandler};
use iroh_blobs::{
    net_protocol::Blobs,
    provider::{self, CustomEventSender, Event, EventSender},
    store::{fs::Store as BlobStore, BaoBlobSize, Map, MapEntry},
    Hash,
};
use iroh_io::AsyncSliceReader;
use n0_future::boxed::BoxFuture;

pub(super) type StoppedHashes = Arc<RwLock<BTreeSet<Hash>>>;

pub(super) fn is_stopped(stopped: &StoppedHashes, hash: &Hash) -> bool {
    stopped
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(hash)
}

pub(super) fn set_stopped(stopped: &StoppedHashes, hash: Hash, value: bool) {
    let mut hashes = stopped
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if value {
        hashes.insert(hash);
    } else {
        hashes.remove(&hash);
    }
}

#[derive(Debug, Clone)]
struct GuardedMap {
    inner: BlobStore,
    stopped: StoppedHashes,
}

impl Map for GuardedMap {
    type Entry = GuardedEntry;

    async fn get(&self, hash: &Hash) -> io::Result<Option<Self::Entry>> {
        if is_stopped(&self.stopped, hash) {
            return Ok(None);
        }
        Ok(self.inner.get(hash).await?.map(|inner| GuardedEntry {
            inner,
            hash: *hash,
            stopped: self.stopped.clone(),
        }))
    }
}

#[derive(Debug, Clone)]
struct GuardedEntry {
    inner: <BlobStore as Map>::Entry,
    hash: Hash,
    stopped: StoppedHashes,
}

impl MapEntry for GuardedEntry {
    fn hash(&self) -> Hash {
        self.hash
    }

    fn size(&self) -> BaoBlobSize {
        self.inner.size()
    }

    fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    async fn outboard(&self) -> io::Result<impl bao_tree::io::fsm::Outboard> {
        self.inner.outboard()
    }

    async fn data_reader(&self) -> io::Result<impl AsyncSliceReader> {
        if is_stopped(&self.stopped, &self.hash) {
            return Err(stopped_error());
        }
        Ok(GuardedReader {
            inner: self.inner.data_reader(),
            hash: self.hash,
            stopped: self.stopped.clone(),
        })
    }
}

struct GuardedReader<R> {
    inner: R,
    hash: Hash,
    stopped: StoppedHashes,
}

impl<R: AsyncSliceReader> AsyncSliceReader for GuardedReader<R> {
    async fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Bytes> {
        if is_stopped(&self.stopped, &self.hash) {
            return Err(stopped_error());
        }
        self.inner.read_at(offset, len).await
    }

    async fn size(&mut self) -> io::Result<u64> {
        if is_stopped(&self.stopped, &self.hash) {
            return Err(stopped_error());
        }
        self.inner.size().await
    }
}

fn stopped_error() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "file sharing stopped")
}

#[derive(Debug, Clone)]
struct BlobEvents {
    tx: async_channel::Sender<Event>,
}

impl CustomEventSender for BlobEvents {
    fn send(&self, event: Event) -> futures_lite::future::Boxed<()> {
        let tx = self.tx.clone();
        Box::pin(async move {
            let _ = tx.send(event).await;
        })
    }

    fn try_send(&self, event: Event) {
        let _ = self.tx.try_send(event);
    }
}

#[derive(Debug, Clone)]
pub struct GuardedBlobProvider {
    blobs: Blobs<BlobStore>,
    store: GuardedMap,
}

impl GuardedBlobProvider {
    pub(super) fn events(tx: async_channel::Sender<Event>) -> EventSender {
        EventSender::from(BlobEvents { tx })
    }

    pub(super) fn new(blobs: Blobs<BlobStore>, stopped: StoppedHashes) -> Self {
        Self {
            store: GuardedMap {
                inner: blobs.store().clone(),
                stopped,
            },
            blobs,
        }
    }
}

impl ProtocolHandler for GuardedBlobProvider {
    fn accept(&self, connecting: Connecting) -> BoxFuture<anyhow::Result<()>> {
        let store = self.store.clone();
        let events = self.blobs.events().clone();
        let rt = self.blobs.rt().clone();
        Box::pin(async move {
            provider::handle_connection(connecting.await?, store, events, rt).await;
            Ok(())
        })
    }

    fn shutdown(&self) -> BoxFuture<()> {
        self.blobs.shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh_blobs::{store::Store, BlobFormat};

    #[tokio::test]
    async fn stopping_blocks_new_requests_and_active_readers() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let store = BlobStore::load(temp.path()).await?;
        let bytes = Bytes::from_static(b"a file to serve");
        let hash = Hash::new(&bytes);
        let _tag = store.import_bytes(bytes.clone(), BlobFormat::Raw).await?;
        let stopped = Arc::new(RwLock::new(BTreeSet::new()));
        let guarded = GuardedMap {
            inner: store,
            stopped: stopped.clone(),
        };
        let entry = guarded.get(&hash).await?.expect("file available initially");
        let mut reader = entry.data_reader().await?;
        assert_eq!(reader.read_at(0, bytes.len()).await?, bytes);
        set_stopped(&stopped, hash, true);
        assert!(guarded.get(&hash).await?.is_none());
        assert_eq!(
            reader.read_at(0, bytes.len()).await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        set_stopped(&stopped, hash, false);
        assert!(guarded.get(&hash).await?.is_some());
        Ok(())
    }
}
