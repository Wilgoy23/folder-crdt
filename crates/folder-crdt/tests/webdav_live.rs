//! WebDav against a real server. Skipped unless one is configured:
//!
//! ```text
//! FOLDER_CRDT_WEBDAV_URL=http://localhost:8765/dav/test   (a folder; it is created)
//! FOLDER_CRDT_WEBDAV_USER=tester
//! FOLDER_CRDT_WEBDAV_PASSWORD=secret
//! cargo test -p folder-crdt --features webdav --test webdav_live
//! ```
//!
//! Every run writes to a fresh file in that folder.

#![cfg(feature = "webdav")]

use folder_crdt::webdav::WebDav;
use folder_crdt::{Expect, Fetched, Put, Remote, StoreError};

fn server(name: &str) -> Option<WebDav> {
    let url = std::env::var("FOLDER_CRDT_WEBDAV_URL").ok()?;
    let user = std::env::var("FOLDER_CRDT_WEBDAV_USER").unwrap_or_default();
    let password = std::env::var("FOLDER_CRDT_WEBDAV_PASSWORD").unwrap_or_default();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    Some(WebDav::new(&url, &format!("{name}-{stamp}.json"), &user, &password).unwrap())
}

#[test]
fn conditional_uploads_and_downloads() {
    let Some(dav) = server("conditional") else {
        eprintln!("FOLDER_CRDT_WEBDAV_URL not set; skipping");
        return;
    };

    assert!(matches!(dav.get(None).unwrap(), Fetched::Missing));

    // Creating, then refusing to create over it
    let Put::Stored(v1) = dav.put(b"one", Expect::Absent).unwrap() else { panic!("create refused") };
    assert!(matches!(dav.put(b"again", Expect::Absent).unwrap(), Put::Stale));

    let Fetched::Found { bytes, version } = dav.get(None).unwrap() else { panic!("not found") };
    assert_eq!(bytes, b"one");
    let v1 = v1.or(version).expect("this server gives ETags");

    assert!(matches!(dav.get(Some(&v1)).unwrap(), Fetched::Unchanged));

    // An upload based on v1 goes through once, then v1 is stale. A different
    // size, since many servers' tags are size plus mtime in whole seconds
    let Put::Stored(v2) = dav.put(b"two!", Expect::Version(&v1)).unwrap() else { panic!("update refused") };
    assert!(matches!(dav.put(b"three", Expect::Version(&v1)).unwrap(), Put::Stale));
    let Fetched::Found { bytes, version } = dav.get(Some(&v1)).unwrap() else { panic!("change not seen") };
    assert_eq!(bytes, b"two!");
    if let (Some(a), Some(b)) = (v2, version) {
        assert_eq!(a, b);
    }
}

#[test]
fn a_wrong_password_is_reported_as_such() {
    let Ok(url) = std::env::var("FOLDER_CRDT_WEBDAV_URL") else { return };
    let dav = WebDav::new(&url, "x.json", "tester", "definitely-wrong").unwrap();
    match dav.get(None) {
        Err(StoreError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}
