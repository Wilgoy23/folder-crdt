//! Keeping a store's data in one file on a server, for devices that have no
//! synced folder to share (phones, mostly).
//!
//! A [`Remote`] is the server: it downloads the file with a version tag and
//! uploads only if the file is still the version the upload was made from.
//! That check replaces the conflict copies a sync client leaves: a device
//! whose upload is refused downloads the newer file, merges it in and tries
//! again, so two devices writing at once both keep their edits.
//! [`WebDav`](crate::webdav::WebDav) is the remote for WebDAV servers.
//!
//! [`RemoteStore`] is a [`Store`] over a remote. Nothing is cached on disk:
//! an app keeps its own local copy (a [`FolderStore`](crate::FolderStore),
//! say) and syncs that with the remote, so it works offline and the server
//! is only ever a mirror.

use std::io;

use serde::Serialize;
use zeroize::Zeroizing;

use crate::codec::{Codec, StoreError};
use crate::merge::Merge;
use crate::store::{Store, SyncOutcome};

/// The server's answer to a download.
#[derive(Debug)]
pub enum Fetched {
    /// The file is still the version asked about.
    Unchanged,
    /// There is no file.
    Missing,
    /// The file, and its version tag if the server gives usable ones.
    Found { bytes: Vec<u8>, version: Option<String> },
}

/// What an upload must find on the server to go ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect<'a> {
    /// No file yet.
    Absent,
    /// The file at exactly this version.
    Version(&'a str),
    /// Anything: for servers that give no version tags, where an upload can
    /// only be checked by reading the file back.
    Any,
}

/// The server's answer to an upload.
#[derive(Debug)]
pub enum Put {
    /// Stored, with the new version tag if the server gave one.
    Stored(Option<String>),
    /// Refused: the file isn't what the upload expected. Another device
    /// wrote first.
    Stale,
}

/// A server holding one file.
pub trait Remote {
    /// Downloads the file, or answers [`Fetched::Unchanged`] when it is
    /// still version `known`.
    fn get(&self, known: Option<&str>) -> Result<Fetched, StoreError>;

    /// Uploads `bytes` if the file is as `expect` says.
    fn put(&self, bytes: &[u8], expect: Expect<'_>) -> Result<Put, StoreError>;
}

/// How many times a sync downloads, merges and uploads again after another
/// device's upload got in first, before giving up until the next sync.
const MAX_ATTEMPTS: usize = 5;

/// What the server held when this store last read or wrote it.
enum Server {
    /// Not asked yet.
    Unknown,
    Absent,
    Holds {
        version: Option<String>,
        bytes: Vec<u8>,
        /// The data in the file, canonical, to tell whether it lacks
        /// anything the device has. Empty after a codec change, which
        /// forces the next sync to upload under the new codec.
        digest: Zeroizing<Vec<u8>>,
    },
}

/// A store kept as one file on a [`Remote`].
///
/// Version tags are only as good as the server makes them. Many (Apache's
/// mod_dav among them) derive the ETag from the size and the modification
/// time in whole seconds, so two writes of the same size within a second
/// share one. An `If-Match` upload can then go through when it shouldn't,
/// and a download asked to skip an unchanged file can skip a changed one.
/// Neither loses data, since every device keeps its own edits and merges
/// them in again, but a change could wait unseen for the next one. So when
/// an upload comes back with the tag it started from, the store stops
/// trusting tags to skip downloads, and [`Self::recheck`] lets the app ask
/// for a full comparison now and then whatever the server.
pub struct RemoteStore<C, R> {
    remote: R,
    codec: C,
    server: Server,
    max_len: u64,
    /// Whether a download may be skipped because the tag hasn't changed
    trust_versions: bool,
}

impl<C, R> RemoteStore<C, R>
where
    C: Codec,
    C::Data: Merge + Serialize,
    R: Remote,
{
    pub fn new(remote: R, codec: C) -> Self {
        RemoteStore { remote, codec, server: Server::Unknown, max_len: 64 << 20, trust_versions: true }
    }

    /// Refuses files longer than `bytes` as damaged. 64 MiB by default.
    pub fn max_len(mut self, bytes: u64) -> Self {
        self.max_len = bytes;
        self
    }

    pub fn remote(&self) -> &R {
        &self.remote
    }

    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Swaps the codec, to write under a new key. The next sync uploads
    /// under it even if nothing else changed. Sync before swapping: a file
    /// another device wrote since must open with the new codec.
    pub fn set_codec(&mut self, codec: C) {
        self.codec = codec;
        if let Server::Holds { digest, .. } = &mut self.server {
            *digest = Zeroizing::new(Vec::new());
        }
    }

    /// Forgets what the server held, so the next sync downloads the file
    /// whole.
    pub fn forget(&mut self) {
        self.server = Server::Unknown;
    }

    /// Makes the next sync download the file whole and compare it, rather
    /// than trust the server to say it hasn't changed.
    pub fn recheck(&mut self) {
        if let Server::Holds { version, .. } = &mut self.server {
            *version = None;
        }
    }

    /// Whether the server's version tags have proved too coarse to skip
    /// downloads by.
    pub fn versions_trusted(&self) -> bool {
        self.trust_versions
    }

    /// The version to ask the server to skip the download for.
    fn known_version(&self) -> Option<&str> {
        match &self.server {
            Server::Holds { version: Some(v), .. } if self.trust_versions => Some(v),
            _ => None,
        }
    }

    fn check_len(&self, bytes: &[u8]) -> Result<(), StoreError> {
        if bytes.len() as u64 > self.max_len {
            return Err(StoreError::Corrupt("the file is larger than the store allows".into()));
        }
        Ok(())
    }

    /// Asks the server what it holds and merges anything new into `data`.
    /// Returns whether the server had changed. A file this codec can't open
    /// is an error, and nothing is recorded, so it is never overwritten.
    fn pull(&mut self, data: &mut C::Data) -> Result<bool, StoreError> {
        match self.remote.get(self.known_version())? {
            Fetched::Unchanged => match self.server {
                Server::Holds { .. } => Ok(false),
                // Only a version this store holds is ever asked about
                _ => Err(StoreError::Corrupt("the server reported no change to a file never read".into())),
            },
            Fetched::Missing => {
                let was_there = !matches!(self.server, Server::Absent);
                self.server = Server::Absent;
                Ok(was_there)
            }
            Fetched::Found { bytes, version } => {
                self.check_len(&bytes)?;
                if let Server::Holds { bytes: ours, version: v, .. } = &mut self.server {
                    // Our own upload, or a server without version tags
                    if *ours == bytes {
                        *v = version;
                        return Ok(false);
                    }
                }
                let theirs = self.codec.decode(&bytes)?;
                let digest = digest(&theirs)?;
                data.merge(theirs);
                self.server = Server::Holds { version, bytes, digest };
                Ok(true)
            }
        }
    }
}

/// `data` in a canonical form, wiped afterwards since it may be secret.
fn digest<T: Serialize>(data: &T) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    serde_json::to_vec(data).map(Zeroizing::new).map_err(|e| StoreError::Corrupt(e.to_string()))
}

impl<C, R> Store for RemoteStore<C, R>
where
    C: Codec,
    C::Data: Merge + Serialize,
    R: Remote,
{
    type Data = C::Data;

    fn load(&mut self) -> Result<C::Data, StoreError> {
        match self.remote.get(None)? {
            Fetched::Found { bytes, version } => {
                self.check_len(&bytes)?;
                let data = self.codec.decode(&bytes)?;
                let digest = digest(&data)?;
                self.server = Server::Holds { version, bytes, digest };
                Ok(data)
            }
            Fetched::Missing => {
                self.server = Server::Absent;
                Err(io::Error::new(io::ErrorKind::NotFound, "there is no file on the server").into())
            }
            Fetched::Unchanged => Err(StoreError::Corrupt("the server reported no change to a file never read".into())),
        }
    }

    fn save(&mut self, data: &mut C::Data) -> Result<(), StoreError> {
        self.sync(data).map(|_| ())
    }

    /// Downloads what other devices wrote and merges it into `data`, then
    /// uploads the result if the server lacks any of it. An upload another
    /// device beat is retried on top of that device's file.
    fn sync(&mut self, data: &mut C::Data) -> Result<SyncOutcome, StoreError> {
        let before = digest(data)?;
        let mut wrote = false;
        let mut settled = false;

        for _ in 0..MAX_ATTEMPTS {
            self.pull(data)?;

            let now = digest(data)?;
            let expect = match &self.server {
                Server::Holds { digest, .. } if **digest == *now => {
                    settled = true;
                    break;
                }
                Server::Holds { version: Some(v), .. } => Expect::Version(v),
                Server::Holds { version: None, .. } => Expect::Any,
                Server::Absent => Expect::Absent,
                Server::Unknown => unreachable!("pull always learns what the server holds"),
            };
            let bytes = self.codec.encode(data)?;
            match self.remote.put(&bytes, expect)? {
                Put::Stored(version) => {
                    // New contents under the old tag: the tags can't tell writes apart
                    if let (Expect::Version(old), Some(new)) = (expect, &version) {
                        if old == new.as_str() {
                            self.trust_versions = false;
                        }
                    }
                    self.server = Server::Holds { version, bytes, digest: now };
                    wrote = true;
                    settled = true;
                    break;
                }
                // Another device's upload got in first: take it in and go again
                Put::Stale => continue,
            }
        }
        if !settled {
            return Err(io::Error::other("the file on the server kept changing; trying again later").into());
        }

        let changed = *digest(data)? != *before;
        Ok(SyncOutcome { changed, wrote, ..SyncOutcome::default() })
    }
}
