//! A [`Remote`] on a WebDAV server: Nextcloud, ownCloud, a NAS, or a hosted
//! service such as Koofr or Fastmail.
//!
//! Only plain HTTP is needed: GET and PUT on the file, with the server's
//! ETag as the version (`If-None-Match` to skip unchanged downloads,
//! `If-Match` and `If-None-Match: *` so an upload never overwrites a file it
//! didn't start from), and MKCOL to create the folder the first time. The
//! file's contents are whatever the store's codec makes of them; with a
//! [`Sealed`](crate::Sealed) codec the server only ever sees ciphertext.

use std::io::{self, Read};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use zeroize::Zeroizing;

use crate::codec::StoreError;
use crate::remote::{Expect, Fetched, Put, Remote};

/// A file on a WebDAV server, with the account that reaches it.
pub struct WebDav {
    folder: String,
    file: String,
    /// The `Authorization` header: the password, base64-encoded
    auth: Zeroizing<String>,
    agent: ureq::Agent,
    max_len: u64,
}

impl WebDav {
    /// The file `name` in the folder at `folder_url`. Refuses anything but
    /// https, except to this machine (`localhost`, for testing): the account
    /// password travels with every request.
    pub fn new(folder_url: &str, name: &str, username: &str, password: &str) -> Result<Self, StoreError> {
        let folder = folder_url.trim().trim_end_matches('/').to_string();
        check_url(&folder)?;
        if name.is_empty() || name.contains('/') {
            return Err(invalid("the file name must not be empty or contain '/'"));
        }
        let credentials = Zeroizing::new(format!("{username}:{password}"));
        let auth = Zeroizing::new(format!("Basic {}", B64.encode(credentials.as_bytes())));
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirects(0)
            .build();
        let file = format!("{folder}/{name}");
        Ok(WebDav { folder, file, auth, agent, max_len: 64 << 20 })
    }

    /// The folder's URL, as given (without a trailing slash).
    pub fn folder_url(&self) -> &str {
        &self.folder
    }

    /// The file's URL.
    pub fn url(&self) -> &str {
        &self.file
    }

    fn request(&self, method: &str, url: &str) -> ureq::Request {
        self.agent.request(method, url).set("Authorization", &self.auth)
    }

    /// Creates the folder. One level only, like `mkdir` without `-p`.
    fn make_folder(&self) -> Result<(), StoreError> {
        match self.request("MKCOL", &format!("{}/", self.folder)).call() {
            Ok(_) => Ok(()),
            // 405: it exists already
            Err(ureq::Error::Status(405, _)) => Ok(()),
            Err(e) => Err(http_error(e)),
        }
    }

    // ureq's error is large, but this never leaves the module
    #[allow(clippy::result_large_err)]
    fn put_once(&self, bytes: &[u8], expect: Expect<'_>) -> Result<ureq::Response, ureq::Error> {
        let req = self.request("PUT", &self.file).set("Content-Type", "application/octet-stream");
        let req = match expect {
            Expect::Absent => req.set("If-None-Match", "*"),
            Expect::Version(v) => req.set("If-Match", v),
            Expect::Any => req,
        };
        req.send_bytes(bytes)
    }
}

impl Remote for WebDav {
    fn get(&self, known: Option<&str>) -> Result<Fetched, StoreError> {
        let mut req = self.request("GET", &self.file);
        if let Some(v) = known {
            req = req.set("If-None-Match", v);
        }
        let resp = match req.call() {
            Ok(resp) => resp,
            Err(ureq::Error::Status(404, _)) => return Ok(Fetched::Missing),
            Err(e) => return Err(http_error(e)),
        };
        if resp.status() == 304 {
            return Ok(Fetched::Unchanged);
        }
        let version = strong_etag(&resp);
        let mut bytes = Vec::new();
        resp.into_reader().take(self.max_len + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > self.max_len {
            return Err(StoreError::Corrupt("the file is larger than the store allows".into()));
        }
        Ok(Fetched::Found { bytes, version })
    }

    fn put(&self, bytes: &[u8], expect: Expect<'_>) -> Result<Put, StoreError> {
        let mut result = self.put_once(bytes, expect);
        // 409 (and 404 on some servers): the folder doesn't exist yet
        if let Err(ureq::Error::Status(404 | 409, _)) = result {
            self.make_folder()?;
            result = self.put_once(bytes, expect);
        }
        match result {
            Ok(resp) => Ok(Put::Stored(strong_etag(&resp))),
            Err(ureq::Error::Status(412, _)) => Ok(Put::Stale),
            Err(e) => Err(http_error(e)),
        }
    }
}

/// The response's ETag, if it is a strong one. `If-Match` compares strongly,
/// so a weak tag could never match and every upload would look stale; with
/// none, the store falls back to reading uploads back.
fn strong_etag(resp: &ureq::Response) -> Option<String> {
    resp.header("ETag").filter(|t| !t.starts_with("W/") && !t.is_empty()).map(str::to_string)
}

fn invalid(why: &str) -> StoreError {
    io::Error::new(io::ErrorKind::InvalidInput, why.to_string()).into()
}

fn check_url(url: &str) -> Result<(), StoreError> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") && lower.len() > "https://".len() {
        return Ok(());
    }
    let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|p| lower.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/'])));
    if local {
        return Ok(());
    }
    Err(invalid("the server address must start with https://"))
}

/// An HTTP failure as a store error, in words a person can act on.
fn http_error(e: ureq::Error) -> StoreError {
    let (kind, why) = match e {
        ureq::Error::Status(401 | 403, _) => {
            (io::ErrorKind::PermissionDenied, "the server refused the username or password".to_string())
        }
        ureq::Error::Status(404 | 409, _) => (io::ErrorKind::NotFound, "that folder doesn't exist on the server".into()),
        ureq::Error::Status(507, _) => (io::ErrorKind::Other, "the server is out of space".into()),
        ureq::Error::Status(code @ 300..=399, _) => {
            (io::ErrorKind::Other, format!("the server redirected the request ({code}); check the address"))
        }
        ureq::Error::Status(code, _) => (io::ErrorKind::Other, format!("the server answered {code}")),
        ureq::Error::Transport(t) => (io::ErrorKind::Other, format!("couldn't reach the server: {t}")),
    };
    io::Error::new(kind, why).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_this_machine() {
        for ok in ["https://dav.example.com/Vault", "https://x/", "http://localhost:8080/dav", "http://127.0.0.1", "http://[::1]:80/"] {
            assert!(check_url(ok.trim_end_matches('/')).is_ok(), "{ok}");
        }
        for bad in ["http://dav.example.com", "http://localhost.evil.com", "ftp://x", "https://", "dav.example.com"] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_file_sits_in_the_folder() {
        let dav = WebDav::new(" https://dav.example.com/files/Vault/ ", "vault.enc", "me", "pw").unwrap();
        assert_eq!(dav.url(), "https://dav.example.com/files/Vault/vault.enc");
        assert_eq!(dav.folder_url(), "https://dav.example.com/files/Vault");
        assert!(WebDav::new("https://x", "a/b", "me", "pw").is_err());
    }
}
