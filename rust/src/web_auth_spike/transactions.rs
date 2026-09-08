//! Transaction prototype exercised with synthetic credentials only. Not wired to
//! Flutter until the platform/account acceptance gates have passed.

use crate::api::booru_auth::{AuthState, WebCookie};
use crate::booru::auth::SessionStore;
use crate::booru::error::{BooruError, BooruErrorCode, BooruResult};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_RECORD_BYTES: u64 = 512 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    source: String,
    commit: u64,
    material: Option<Material>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Material {
    cookies: Vec<WebCookie>,
    user_agent: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Attempt(u64);

struct Pending {
    id: Attempt,
    revision: u32,
    deadline: Instant,
}

struct OperationState {
    sequence: u64,
    pending: Option<Pending>,
    committed: Option<(Attempt, AuthState)>,
}

/// One coordinator per source in this prototype. A future application owner must
/// share it across engine recreation and impose global collector exclusivity.
pub struct TransactionPrototype {
    source: String,
    path: PathBuf,
    live: Arc<SessionStore>,
    operation: Mutex<OperationState>,
}

impl TransactionPrototype {
    /// `directory` and `source` come from trusted application configuration.
    pub fn new(directory: &Path, source: &str, live: Arc<SessionStore>) -> BooruResult<Self> {
        if !matches!(source, "anime_pictures" | "konachan_com_spike") {
            return Err(BooruError::invalid_request(None));
        }
        Ok(Self {
            source: source.into(),
            path: directory.join(format!("{source}.json")),
            live,
            operation: Mutex::new(OperationState {
                sequence: 0,
                pending: None,
                committed: None,
            }),
        })
    }

    pub fn begin(&self, timeout: Duration) -> BooruResult<Attempt> {
        let mut state = self.operation.lock().map_err(|_| self.storage_error())?;
        if state.pending.is_some() {
            return Err(BooruError::invalid_request(Some(&self.source)));
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or_else(|| self.storage_error())?;
        let id = Attempt(state.sequence);
        state.pending = Some(Pending {
            id,
            revision: self.live.revision(&self.source),
            deadline: Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| self.storage_error())?,
        });
        Ok(id)
    }

    /// Construct a separate verification context. The active session is untouched.
    pub fn candidate(
        &self,
        cookies: Vec<WebCookie>,
        user_agent: String,
    ) -> BooruResult<Arc<SessionStore>> {
        let selected = if self.source == "anime_pictures" {
            super::SpikeSource::AnimePictures
        } else {
            super::SpikeSource::KonachanCom
        };
        if cookies
            .iter()
            .any(|cookie| !super::cookie_domain_allowed_for(selected, &cookie.domain))
        {
            return Err(BooruError::invalid_request(Some(&self.source)));
        }
        let candidate = Arc::new(SessionStore::default());
        candidate.set_web_session(&self.source, cookies, user_agent)?;
        Ok(candidate)
    }

    /// Called only after the verifier has marked the detached context Verified.
    /// Rejected verification never calls this method, so the window can retry.
    pub fn commit(&self, id: Attempt, candidate: &SessionStore) -> BooruResult<AuthState> {
        let mut state = self.operation.lock().map_err(|_| self.storage_error())?;
        if let Some((committed, result)) = &state.committed {
            if *committed == id {
                return Ok(result.clone());
            }
        }
        let pending = state
            .pending
            .as_ref()
            .filter(|pending| pending.id == id)
            .ok_or_else(|| self.cancelled())?;
        if Instant::now() >= pending.deadline {
            state.pending = None;
            return Err(BooruError::new(
                BooruErrorCode::Timeout,
                Some(&self.source),
                "booru.error.timeout",
            ));
        }
        let snapshot = candidate
            .verified_web_snapshot(&self.source)?
            .ok_or_else(|| self.cancelled())?;
        // Revalidate scope even if a caller did not use candidate().
        self.candidate(snapshot.cookies.clone(), snapshot.user_agent.clone())?;
        let record = Record {
            version: 1,
            source: self.source.clone(),
            commit: id.0,
            material: Some(Material {
                cookies: snapshot.cookies.clone(),
                user_agent: snapshot.user_agent.clone(),
            }),
        };
        let response = AuthState {
            source_id: self.source.clone(),
            status: crate::api::booru_auth::AuthStatus::Verified,
            username: None,
            auth_revision: pending.revision.wrapping_add(1),
        };
        let result =
            self.live
                .commit_web_snapshot(&self.source, pending.revision, snapshot, || {
                    self.replace(&record, Some(pending.deadline))
                })?;
        state.committed = Some((id, result));
        state.pending = None;
        Ok(response)
    }

    /// Returns an already committed result rather than claiming it was cancelled.
    pub fn cancel(&self, id: Attempt) -> BooruResult<Option<AuthState>> {
        let mut state = self.operation.lock().map_err(|_| self.storage_error())?;
        if let Some((committed, result)) = &state.committed {
            if *committed == id {
                return Ok(Some(result.clone()));
            }
        }
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            state.pending = None;
        }
        Ok(None)
    }

    /// A tombstone prevents legacy credentials from being restored after logout.
    pub fn logout(&self) -> BooruResult<()> {
        let mut state = self.operation.lock().map_err(|_| self.storage_error())?;
        state.pending = None;
        let record = Record {
            version: 1,
            source: self.source.clone(),
            commit: state.sequence,
            material: None,
        };
        self.live
            .clear_with_persistence(&self.source, || self.replace(&record, None))?;
        state.committed = None;
        Ok(())
    }

    /// The application owner calls this before changing network configuration.
    pub fn invalidate_pending(&self) -> BooruResult<()> {
        self.operation
            .lock()
            .map_err(|_| self.storage_error())?
            .pending = None;
        Ok(())
    }

    /// Outer None: no record (migration is possible). Inner None: logout marker.
    /// Restored material is detached and unverified; it is never activated here.
    pub fn restore(&self) -> BooruResult<Option<Option<Arc<SessionStore>>>> {
        let _state = self.operation.lock().map_err(|_| self.storage_error())?;
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(self.storage_error()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| self.storage_error())?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(self.storage_error());
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| self.storage_error())?;
        if record.version != 1 || record.source != self.source {
            return Err(self.storage_error());
        }
        Ok(Some(match record.material {
            Some(material) => Some(self.candidate(material.cookies, material.user_agent)?),
            None => None,
        }))
    }

    fn replace(&self, record: &Record, deadline: Option<Instant>) -> BooruResult<()> {
        let bytes = serde_json::to_vec(record).map_err(|_| self.storage_error())?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(self.storage_error());
        }
        let directory = self.path.parent().ok_or_else(|| self.storage_error())?;
        std::fs::create_dir_all(directory).map_err(|_| self.storage_error())?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).map_err(|_| self.storage_error())?;
        temporary
            .write_all(&bytes)
            .map_err(|_| self.storage_error())?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| self.storage_error())?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(BooruError::new(
                BooruErrorCode::Timeout,
                Some(&self.source),
                "booru.error.timeout",
            ));
        }
        // tempfile uses the platform's atomic replacement; never delete the old file.
        temporary
            .persist(&self.path)
            .map_err(|_| self.storage_error())?;
        Ok(())
    }

    fn storage_error(&self) -> BooruError {
        BooruError::new(
            BooruErrorCode::Internal,
            Some(&self.source),
            "booru.error.storageFailed",
        )
    }
    fn cancelled(&self) -> BooruError {
        BooruError::new(
            BooruErrorCode::Cancelled,
            Some(&self.source),
            "booru.error.cancelled",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::booru_auth::AuthStatus;

    fn candidate(owner: &TransactionPrototype, value: &str) -> Arc<SessionStore> {
        let result = owner
            .candidate(
                vec![WebCookie {
                    name: "session".into(),
                    value: value.into(),
                    domain: ".anime-pictures.net".into(),
                    path: "/".into(),
                    secure: true,
                    expires_at_iso8601: None,
                }],
                "Fixture-Browser".into(),
            )
            .unwrap();
        result.mark_verified("anime_pictures").unwrap();
        result
    }

    #[test]
    fn commit_restore_logout_and_duplicate_completion() {
        let dir = tempfile::tempdir().unwrap();
        let live = Arc::new(SessionStore::default());
        let owner = TransactionPrototype::new(dir.path(), "anime_pictures", live.clone()).unwrap();
        assert!(owner.restore().unwrap().is_none());
        let id = owner.begin(Duration::from_secs(60)).unwrap();
        let detached = candidate(&owner, "abc=def==");
        assert_eq!(
            live.state("anime_pictures").unwrap().status,
            AuthStatus::NotConfigured
        );
        let committed = owner.commit(id, &detached).unwrap();
        assert_eq!(committed.auth_revision, 1);
        assert_eq!(owner.commit(id, &detached).unwrap(), committed);
        assert_eq!(owner.cancel(id).unwrap(), Some(committed));
        let restarted = TransactionPrototype::new(
            dir.path(),
            "anime_pictures",
            Arc::new(SessionStore::default()),
        )
        .unwrap();
        let restored = restarted.restore().unwrap().unwrap().unwrap();
        assert_eq!(
            restored.state("anime_pictures").unwrap().status,
            AuthStatus::ConfiguredUnverified
        );
        assert_eq!(
            restored
                .web_snapshot("anime_pictures")
                .unwrap()
                .unwrap()
                .cookies[0]
                .value,
            "abc=def=="
        );
        owner.logout().unwrap();
        assert_eq!(
            live.state("anime_pictures").unwrap().status,
            AuthStatus::NotConfigured
        );
        assert!(restarted.restore().unwrap().unwrap().is_none());
    }

    #[test]
    fn cancelled_expired_or_superseded_candidates_cannot_commit() {
        let dir = tempfile::tempdir().unwrap();
        let live = Arc::new(SessionStore::default());
        let owner = TransactionPrototype::new(dir.path(), "anime_pictures", live.clone()).unwrap();
        let detached = candidate(&owner, "new");
        let id = owner.begin(Duration::from_secs(60)).unwrap();
        owner.cancel(id).unwrap();
        assert!(owner.commit(id, &detached).is_err());
        let id = owner.begin(Duration::ZERO).unwrap();
        assert_eq!(
            owner.commit(id, &detached).unwrap_err().code,
            BooruErrorCode::Timeout
        );
        let id = owner.begin(Duration::from_secs(60)).unwrap();
        live.clear("anime_pictures").unwrap();
        assert_eq!(
            owner.commit(id, &detached).unwrap_err().code,
            BooruErrorCode::Cancelled
        );
        assert!(!owner.path.exists());
        owner.cancel(id).unwrap();
        let id = owner.begin(Duration::from_secs(60)).unwrap();
        owner.logout().unwrap();
        assert!(owner.commit(id, &detached).is_err());
        assert!(owner.restore().unwrap().unwrap().is_none());
    }

    #[test]
    fn network_change_and_untrusted_scope_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let owner = TransactionPrototype::new(
            dir.path(),
            "anime_pictures",
            Arc::new(SessionStore::default()),
        )
        .unwrap();
        let id = owner.begin(Duration::from_secs(60)).unwrap();
        owner.invalidate_pending().unwrap();
        assert!(owner.commit(id, &candidate(&owner, "new")).is_err());
        for domain in [
            "net",
            "anime-pictures.net.attacker.test",
            "unknown.anime-pictures.net",
        ] {
            assert!(owner
                .candidate(
                    vec![WebCookie {
                        name: "session".into(),
                        value: "fixture".into(),
                        domain: domain.into(),
                        path: "/".into(),
                        secure: true,
                        expires_at_iso8601: None,
                    }],
                    "Fixture".into()
                )
                .is_err());
        }
    }

    #[test]
    fn corrupt_and_newer_records_are_retained_but_not_restored() {
        let dir = tempfile::tempdir().unwrap();
        let owner = TransactionPrototype::new(
            dir.path(),
            "anime_pictures",
            Arc::new(SessionStore::default()),
        )
        .unwrap();
        for bytes in [
            b"{".to_vec(),
            br#"{"version":2,"source":"anime_pictures","commit":1,"material":null}"#.to_vec(),
            vec![b' '; MAX_RECORD_BYTES as usize + 1],
        ] {
            std::fs::write(&owner.path, &bytes).unwrap();
            assert!(owner.restore().is_err());
            assert_eq!(std::fs::read(&owner.path).unwrap(), bytes);
        }
    }

    #[test]
    fn rejected_and_failed_save_keep_old_session_and_allow_retry() {
        let dir = tempfile::tempdir().unwrap();
        let live = Arc::new(SessionStore::default());
        let owner = TransactionPrototype::new(dir.path(), "anime_pictures", live.clone()).unwrap();
        let first = owner.begin(Duration::from_secs(60)).unwrap();
        let old = owner.commit(first, &candidate(&owner, "old")).unwrap();
        let old_file = std::fs::read(&owner.path).unwrap();
        let id = owner.begin(Duration::from_secs(60)).unwrap();
        let detached = candidate(&owner, "new");
        detached.mark_rejected("anime_pictures").unwrap();
        assert!(owner.commit(id, &detached).is_err());
        assert_eq!(std::fs::read(&owner.path).unwrap(), old_file);
        detached.mark_verified("anime_pictures").unwrap();
        // Deny deletion/replacement by keeping the existing Windows file open.
        use std::os::windows::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&owner.path)
            .unwrap();
        assert!(owner.commit(id, &detached).is_err());
        assert_eq!(live.state("anime_pictures").unwrap(), old);
        assert_eq!(std::fs::read(&owner.path).unwrap(), old_file);
        drop(lock);
        assert_eq!(
            owner.commit(id, &detached).unwrap().auth_revision,
            old.auth_revision + 1
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
