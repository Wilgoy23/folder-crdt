//! RemoteStore against an in-memory server, with devices simulated as
//! separate stores on one server.

use std::cell::RefCell;
use std::rc::Rc;

use folder_crdt::{
    Cipher, DeleteWins, Expect, Fetched, Hlc, Json, KeyMismatch, Lww, Merge, Put, Record, RecordSet, Remote,
    RemoteStore, Sealed, Store, StoreError, Unknown,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Merge, Record)]
#[record(clock = Hlc)]
struct Item {
    title: Lww<String, Hlc>,
    #[serde(flatten)]
    extra: Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Merge)]
struct Todo {
    lists: RecordSet<String, Item, DeleteWins>,
    #[serde(flatten)]
    extra: Unknown,
}

fn with(items: &[(&str, &str, u64, u64)]) -> Todo {
    let mut t = Todo::default();
    for (id, title, ms, device) in items {
        let item = Item { title: Lww::new(title.to_string(), Hlc::new(*ms, 0, *device)), extra: Unknown::default() };
        t.lists.upsert(id.to_string(), item);
    }
    t
}

fn titles(t: &Todo) -> Vec<String> {
    let mut v: Vec<String> = t.lists.iter().map(|(_, i)| i.title.get().unwrap().clone()).collect();
    v.sort();
    v
}

// ── An in-memory server ───────────────────────────────────────────────────

/// Another device's write, run in the gap before an upload is checked
type Meanwhile = Box<dyn FnOnce(&mut State)>;

#[derive(Default)]
struct State {
    file: Option<(Vec<u8>, u64)>,
    next_version: u64,
    /// Give out no version tags, like some servers
    no_versions: bool,
    /// Keep the tag when a write leaves the size unchanged, like servers
    /// whose ETag is size plus modification time in whole seconds
    coarse: bool,
    gets: usize,
    puts: usize,
    /// Runs once just before the next PUT is checked: another device
    /// writing in the gap between this one's download and upload
    before_put: Option<Meanwhile>,
}

impl State {
    fn store(&mut self, bytes: Vec<u8>) -> u64 {
        if let Some((old, v)) = &self.file {
            if self.coarse && old.len() == bytes.len() {
                let v = *v;
                self.file = Some((bytes, v));
                return v;
            }
        }
        self.next_version += 1;
        self.file = Some((bytes, self.next_version));
        self.next_version
    }

    fn tag(&self, v: u64) -> Option<String> {
        (!self.no_versions).then(|| format!("\"v{v}\""))
    }
}

#[derive(Clone, Default)]
struct Memory(Rc<RefCell<State>>);

impl Remote for Memory {
    fn get(&self, known: Option<&str>) -> Result<Fetched, StoreError> {
        let mut s = self.0.borrow_mut();
        s.gets += 1;
        Ok(match &s.file {
            None => Fetched::Missing,
            Some((_, v)) if known.is_some() && s.tag(*v).as_deref() == known => Fetched::Unchanged,
            Some((bytes, v)) => Fetched::Found { bytes: bytes.clone(), version: s.tag(*v) },
        })
    }

    fn put(&self, bytes: &[u8], expect: Expect<'_>) -> Result<Put, StoreError> {
        let mut s = self.0.borrow_mut();
        s.puts += 1;
        if let Some(f) = s.before_put.take() {
            f(&mut s);
        }
        let ok = match (&s.file, expect) {
            (None, Expect::Absent) | (_, Expect::Any) => true,
            (Some((_, v)), Expect::Version(want)) => s.tag(*v).as_deref() == Some(want),
            _ => false,
        };
        if !ok {
            return Ok(Put::Stale);
        }
        let v = s.store(bytes.to_vec());
        Ok(Put::Stored(s.tag(v)))
    }
}

fn device(server: &Memory) -> RemoteStore<Json<Todo>, Memory> {
    RemoteStore::new(server.clone(), Json::new("todo", 1))
}

/// What another device would upload: `data` as the file bytes.
fn bytes_of(data: &Todo) -> Vec<u8> {
    use folder_crdt::Codec;
    Json::<Todo>::new("todo", 1).encode(data).unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[test]
fn the_first_sync_uploads_and_a_second_device_receives_it() {
    let server = Memory::default();
    let mut a = device(&server);
    let mut a_data = with(&[("1", "milk", 1_000, 1)]);
    let out = a.sync(&mut a_data).unwrap();
    assert!(out.wrote && !out.changed);

    let mut b = device(&server);
    let mut b_data = Todo::default();
    let out = b.sync(&mut b_data).unwrap();
    assert!(out.changed && !out.wrote, "nothing to send back");
    assert_eq!(titles(&b_data), ["milk"]);
}

#[test]
fn edits_from_both_devices_survive() {
    let server = Memory::default();
    let (mut a, mut b) = (device(&server), device(&server));
    let mut a_data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut a_data).unwrap();
    let mut b_data = Todo::default();
    b.sync(&mut b_data).unwrap();

    // Both edit offline, then sync one after the other
    a_data.merge(with(&[("2", "eggs", 2_000, 1)]));
    b_data.merge(with(&[("3", "bread", 2_000, 2)]));
    a.sync(&mut a_data).unwrap();
    b.sync(&mut b_data).unwrap();
    a.sync(&mut a_data).unwrap();

    assert_eq!(titles(&a_data), ["bread", "eggs", "milk"]);
    assert_eq!(a_data, b_data);
}

#[test]
fn an_upload_another_device_beat_is_merged_and_retried() {
    let server = Memory::default();
    let mut a = device(&server);
    let mut a_data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut a_data).unwrap();

    a_data.merge(with(&[("2", "eggs", 2_000, 1)]));
    // B uploads between A's download and A's upload
    let theirs = bytes_of(&with(&[("1", "milk", 1_000, 1), ("3", "bread", 2_500, 2)]));
    server.0.borrow_mut().before_put = Some(Box::new(move |s: &mut State| {
        s.store(theirs);
    }));

    let out = a.sync(&mut a_data).unwrap();
    assert!(out.changed && out.wrote);
    assert_eq!(titles(&a_data), ["bread", "eggs", "milk"]);
    // What the server holds now is the merge, not either side alone
    let mut check = Todo::default();
    device(&server).sync(&mut check).unwrap();
    assert_eq!(check, a_data);
}

#[test]
fn a_device_creating_the_file_second_merges_instead_of_overwriting() {
    let server = Memory::default();
    let mut a = device(&server);
    let mut a_data = with(&[("1", "milk", 1_000, 1)]);
    // A thinks the server is empty; B creates the file just before A's upload
    let theirs = bytes_of(&with(&[("2", "eggs", 1_500, 2)]));
    server.0.borrow_mut().before_put = Some(Box::new(move |s: &mut State| {
        s.store(theirs);
    }));
    a.sync(&mut a_data).unwrap();
    assert_eq!(titles(&a_data), ["eggs", "milk"]);
}

#[test]
fn an_unchanged_server_costs_one_conditional_download() {
    let server = Memory::default();
    let mut a = device(&server);
    let mut data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut data).unwrap();
    let (gets, puts) = (server.0.borrow().gets, server.0.borrow().puts);

    let out = a.sync(&mut data).unwrap();
    assert!(!out.changed && !out.wrote);
    assert_eq!(server.0.borrow().gets, gets + 1);
    assert_eq!(server.0.borrow().puts, puts, "nothing uploaded");
}

#[test]
fn a_server_without_versions_still_syncs() {
    let server = Memory::default();
    server.0.borrow_mut().no_versions = true;
    let (mut a, mut b) = (device(&server), device(&server));
    let mut a_data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut a_data).unwrap();
    // Reading back its own upload is not a change
    let out = a.sync(&mut a_data).unwrap();
    assert!(!out.changed && !out.wrote);

    let mut b_data = with(&[("2", "eggs", 2_000, 2)]);
    b.sync(&mut b_data).unwrap();
    a.sync(&mut a_data).unwrap();
    assert_eq!(titles(&a_data), ["eggs", "milk"]);
}

#[test]
fn coarse_tags_are_noticed_and_downloads_no_longer_skipped() {
    let server = Memory::default();
    server.0.borrow_mut().coarse = true;
    let mut a = device(&server);
    // Same-length edits: the server's tag doesn't move
    let mut data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut data).unwrap();
    assert!(a.versions_trusted());
    data.merge(with(&[("1", "silk", 2_000, 1)]));
    a.sync(&mut data).unwrap();
    assert!(!a.versions_trusted(), "an upload came back with the tag it started from");

    // Another device's same-length write is still seen
    server.0.borrow_mut().store(bytes_of(&with(&[("1", "salt", 3_000, 2)])));
    assert!(a.sync(&mut data).unwrap().changed);
    assert_eq!(titles(&data), ["salt"]);
}

#[test]
fn a_recheck_finds_a_change_the_tag_hid() {
    let server = Memory::default();
    server.0.borrow_mut().coarse = true;
    let mut a = device(&server);
    let mut data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut data).unwrap();

    // Another device writes the same size under the same tag
    server.0.borrow_mut().store(bytes_of(&with(&[("1", "silk", 2_000, 2)])));
    assert!(!a.sync(&mut data).unwrap().changed, "the tag says unchanged");
    a.recheck();
    assert!(a.sync(&mut data).unwrap().changed);
    assert_eq!(titles(&data), ["silk"]);
}

#[test]
fn a_deleted_file_is_uploaded_again() {
    let server = Memory::default();
    let mut a = device(&server);
    let mut data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut data).unwrap();
    server.0.borrow_mut().file = None;

    let out = a.sync(&mut data).unwrap();
    assert!(out.wrote);
    assert!(server.0.borrow().file.is_some());
}

#[test]
fn a_server_that_never_settles_gives_up_without_losing_data() {
    let server = Memory::default();
    let mut a = device(&server);
    let mut data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut data).unwrap();
    data.merge(with(&[("2", "eggs", 2_000, 1)]));

    // Every upload is beaten by yet another write
    struct Busy(Memory, usize);
    impl Remote for Busy {
        fn get(&self, known: Option<&str>) -> Result<Fetched, StoreError> {
            self.0.get(known)
        }
        fn put(&self, _: &[u8], _: Expect<'_>) -> Result<Put, StoreError> {
            let n = self.1 + self.0 .0.borrow().puts;
            self.0 .0.borrow_mut().puts += 1;
            self.0 .0.borrow_mut().store(bytes_of(&with(&[("1", &format!("milk {n}"), 5_000 + n as u64, 2)])));
            Ok(Put::Stale)
        }
    }
    let mut busy = RemoteStore::new(Busy(server.clone(), 0), Json::<Todo>::new("todo", 1));
    assert!(busy.sync(&mut data).is_err());
    assert!(data.lists.get("2").is_some(), "the local edit is still there");
}

// ── Keys ──────────────────────────────────────────────────────────────────

struct Xor(u8);

impl Cipher for Xor {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
        Ok(std::iter::once(self.0).chain(plaintext.iter().map(|b| b ^ self.0)).collect())
    }
    fn open(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, KeyMismatch> {
        match sealed.split_first() {
            Some((&k, rest)) if k == self.0 => Ok(Zeroizing::new(rest.iter().map(|b| b ^ self.0).collect())),
            _ => Err(KeyMismatch),
        }
    }
}

fn sealed(server: &Memory, key: u8) -> RemoteStore<Sealed<Todo, Xor>, Memory> {
    RemoteStore::new(server.clone(), Sealed::new("todo", 1, Xor(key)))
}

#[test]
fn a_file_under_another_key_is_never_overwritten() {
    let server = Memory::default();
    let mut a = sealed(&server, 7);
    let mut data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut data).unwrap();
    let before = server.0.borrow().file.clone();

    let mut b = sealed(&server, 9);
    let mut other = with(&[("2", "eggs", 2_000, 2)]);
    assert!(matches!(b.sync(&mut other), Err(StoreError::KeyMismatch)));
    assert_eq!(server.0.borrow().file, before);
}

#[test]
fn a_new_codec_is_uploaded_even_with_nothing_else_changed() {
    let server = Memory::default();
    let mut a = sealed(&server, 7);
    let mut data = with(&[("1", "milk", 1_000, 1)]);
    a.sync(&mut data).unwrap();

    a.set_codec(Sealed::new("todo", 1, Xor(9)));
    assert!(a.sync(&mut data).unwrap().wrote);
    // Now only the new key opens it
    assert!(sealed(&server, 7).load().is_err());
    assert_eq!(titles(&sealed(&server, 9).load().unwrap()), ["milk"]);
}
