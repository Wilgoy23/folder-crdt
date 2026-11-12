# folder-crdt

Keeps an app's data in step across devices through a folder that a sync
client (iCloud Drive, OneDrive, Dropbox, Syncthing) already moves between
them. No server, no account: each device writes a file into the folder, and
merges in what the others wrote, including the conflict copies sync clients
leave behind.

Every merge is a *join* (commutative, associative, idempotent), so devices
that have seen the same edits always end up with the same data, whatever
order the copies arrive in. Property tests check this on every run.

## Crates

| Crate | What it is |
|---|---|
| [`folder-crdt`](crates/folder-crdt) | Field types, records, the file store and codecs |
| [`folder-crdt-derive`](crates/folder-crdt-derive) | `#[derive(Merge, Record)]`, re-exported by `folder-crdt` |

## Use

```rust
use folder_crdt::{Counter, DeleteWins, FolderStore, Hlc, Json, Lww, Merge, Record, RecordSet, Store, Unknown};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Merge, Record)]
#[record(clock = Hlc)]
struct Bookmark {
    title: Lww<String, Hlc>,
    url: Lww<String, Hlc>,
    visits: Counter,
    #[serde(flatten)]
    extra: Unknown, // fields from newer builds survive a save from this one
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Merge)]
struct Data {
    bookmarks: RecordSet<String, Bookmark, DeleteWins>,
    #[serde(flatten)]
    extra: Unknown,
}

let mut store = FolderStore::new(shared_dir, "bookmarks", "json", Json::<Data>::new("my-app", 1));
let mut data = store.load()?;
// ... edit ...
store.save(&mut data)?;          // merges first if another device wrote since
if store.sync(&mut data)?.changed {
    // refresh the UI
}
```

To encrypt the file, use `Sealed` with your own `Cipher` instead of `Json`.
The crate has no cryptography of its own: only the file header and whatever
your cipher puts in the clear reach the disk unencrypted.

## Status

Pre-release (0.0.0). Used by [Vault](https://github.com/Wilgoy23/Vault).

## Licence

MIT or Apache-2.0, at your option.
