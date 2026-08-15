//! Persistent provider replay sidecar and response-item collector.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use philo::api::stable as sdk;
use philo_agent_runtime::{
    ModelCallSnapshot, ModelError, ModelMessage, ModelToolResultOutcome, UserPart,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SIDECAR_DIR: &str = "model-replay";
const LOCK_FILE: &str = "lock";
const RECORD_SUFFIX: &str = ".replay";
const GENERATION_SCHEMA_V1: u32 = 1;
const GENERATION_SCHEMA_V2: u32 = 2;
const DEFAULT_MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// Retention and quota policy for one model replay sidecar store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayStorePolicy {
    pub max_session_bytes: u64,
    pub ttl: Duration,
    pub orphan_grace: Duration,
}

impl Default for ReplayStorePolicy {
    fn default() -> Self {
        Self {
            max_session_bytes: DEFAULT_MAX_SESSION_BYTES,
            ttl: DEFAULT_TTL,
            orphan_grace: DEFAULT_ORPHAN_GRACE,
        }
    }
}

/// Stable, payload-free category for sidecar failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayStoreErrorCode {
    Io,
    Corrupted,
    QuotaExceeded,
    Conflict,
}

/// A redacted replay sidecar failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayStoreError {
    code: ReplayStoreErrorCode,
}

impl ReplayStoreError {
    /// Creates a payload-free store error for custom store implementations.
    pub const fn new(code: ReplayStoreErrorCode) -> Self {
        Self { code }
    }

    pub const fn code(&self) -> ReplayStoreErrorCode {
        self.code
    }
}

impl fmt::Display for ReplayStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.code {
            ReplayStoreErrorCode::Io => "model replay sidecar I/O failed",
            ReplayStoreErrorCode::Corrupted => "model replay sidecar is corrupted",
            ReplayStoreErrorCode::QuotaExceeded => "model replay sidecar quota exceeded",
            ReplayStoreErrorCode::Conflict => "model replay sidecar generation conflict",
        })
    }
}

impl std::error::Error for ReplayStoreError {}

/// One opaque generation persisted by a [`ModelReplayStore`].
#[derive(Clone)]
pub struct ReplayStoreBlob {
    id: String,
    payload: Vec<u8>,
}

impl ReplayStoreBlob {
    pub fn new(id: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            id: id.into(),
            payload,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl fmt::Debug for ReplayStoreBlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayStoreBlob")
            .field("id", &self.id)
            .field("payload_len", &self.payload.len())
            .field("payload", &"<redacted>")
            .finish()
    }
}

/// Narrow persistence seam for target-bound provider replay generations.
pub trait ModelReplayStore: Send + Sync + fmt::Debug {
    fn policy(&self) -> ReplayStorePolicy;

    fn load(&self, session_id: &str) -> Result<Vec<ReplayStoreBlob>, ReplayStoreError>;

    fn commit(&self, session_id: &str, blob: ReplayStoreBlob) -> Result<(), ReplayStoreError>;

    fn remove(&self, session_id: &str, generation_ids: &[String]) -> Result<(), ReplayStoreError>;

    fn delete_session(&self, session_id: &str) -> Result<(), ReplayStoreError>;
}

/// Process-local store used when callers do not configure durable replay.
pub struct MemoryModelReplayStore {
    policy: ReplayStorePolicy,
    sessions: Mutex<HashMap<String, Vec<ReplayStoreBlob>>>,
}

impl MemoryModelReplayStore {
    pub fn new(policy: ReplayStorePolicy) -> Self {
        Self {
            policy,
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemoryModelReplayStore {
    fn default() -> Self {
        Self::new(ReplayStorePolicy::default())
    }
}

impl fmt::Debug for MemoryModelReplayStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryModelReplayStore")
            .field("policy", &self.policy)
            .field("contents", &"<redacted>")
            .finish()
    }
}

impl ModelReplayStore for MemoryModelReplayStore {
    fn policy(&self) -> ReplayStorePolicy {
        self.policy
    }

    fn load(&self, session_id: &str) -> Result<Vec<ReplayStoreBlob>, ReplayStoreError> {
        Ok(self
            .sessions
            .lock()
            .map_err(|_| ReplayStoreError::new(ReplayStoreErrorCode::Io))?
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    fn commit(&self, session_id: &str, blob: ReplayStoreBlob) -> Result<(), ReplayStoreError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ReplayStoreError::new(ReplayStoreErrorCode::Io))?;
        let records = sessions.entry(session_id.to_owned()).or_default();
        if let Some(existing) = records.iter().find(|record| record.id == blob.id) {
            return if existing.payload == blob.payload {
                Ok(())
            } else {
                Err(ReplayStoreError::new(ReplayStoreErrorCode::Conflict))
            };
        }
        let current = records.iter().fold(0_u64, |total, record| {
            total.saturating_add(len_u64(record.payload.len()))
        });
        if current.saturating_add(len_u64(blob.payload.len())) > self.policy.max_session_bytes {
            return Err(ReplayStoreError::new(ReplayStoreErrorCode::QuotaExceeded));
        }
        records.push(blob);
        Ok(())
    }

    fn remove(&self, session_id: &str, generation_ids: &[String]) -> Result<(), ReplayStoreError> {
        let remove = generation_ids.iter().collect::<HashSet<_>>();
        if let Some(records) = self
            .sessions
            .lock()
            .map_err(|_| ReplayStoreError::new(ReplayStoreErrorCode::Io))?
            .get_mut(session_id)
        {
            records.retain(|record| !remove.contains(&record.id));
        }
        Ok(())
    }

    fn delete_session(&self, session_id: &str) -> Result<(), ReplayStoreError> {
        self.sessions
            .lock()
            .map_err(|_| ReplayStoreError::new(ReplayStoreErrorCode::Io))?
            .remove(session_id);
        Ok(())
    }
}

/// Atomic filesystem sidecar stored below each JSONL session directory.
pub struct FileModelReplayStore {
    data_root: PathBuf,
    policy: ReplayStorePolicy,
}

impl FileModelReplayStore {
    pub fn open(data_root: impl Into<PathBuf>) -> Result<Self, ReplayStoreError> {
        Self::with_policy(data_root, ReplayStorePolicy::default())
    }

    pub fn with_policy(
        data_root: impl Into<PathBuf>,
        policy: ReplayStorePolicy,
    ) -> Result<Self, ReplayStoreError> {
        let data_root = data_root.into();
        fs::create_dir_all(&data_root).map_err(|_| store_io())?;
        restrict_directory(&data_root)?;
        Ok(Self { data_root, policy })
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.data_root.join(session_dir_name(session_id))
    }

    fn replay_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join(SIDECAR_DIR)
    }

    fn prepare_replay_dir(&self, session_id: &str) -> Result<PathBuf, ReplayStoreError> {
        let session = self.session_dir(session_id);
        fs::create_dir_all(&session).map_err(|_| store_io())?;
        restrict_directory(&session)?;
        let replay = session.join(SIDECAR_DIR);
        fs::create_dir_all(&replay).map_err(|_| store_io())?;
        restrict_directory(&replay)?;
        Ok(replay)
    }

    fn acquire_lock(&self, replay_dir: &Path) -> Result<File, ReplayStoreError> {
        let lock = secure_open(replay_dir.join(LOCK_FILE), false)?;
        lock.lock().map_err(|_| store_io())?;
        Ok(lock)
    }

    fn record_paths(replay_dir: &Path) -> Result<Vec<PathBuf>, ReplayStoreError> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(replay_dir).map_err(|_| store_io())? {
            let entry = entry.map_err(|_| store_io())?;
            if !entry.file_type().map_err(|_| store_io())?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("replay") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn cleanup_temp_files(replay_dir: &Path) -> Result<(), ReplayStoreError> {
        let mut removed = false;
        for entry in fs::read_dir(replay_dir).map_err(|_| store_io())? {
            let entry = entry.map_err(|_| store_io())?;
            if !entry.file_type().map_err(|_| store_io())?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("tmp") {
                fs::remove_file(path).map_err(|_| store_io())?;
                removed = true;
            }
        }
        if removed {
            sync_directory(replay_dir)?;
        }
        Ok(())
    }
}

impl fmt::Debug for FileModelReplayStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileModelReplayStore")
            .field("data_root", &self.data_root)
            .field("policy", &self.policy)
            .finish()
    }
}

impl ModelReplayStore for FileModelReplayStore {
    fn policy(&self) -> ReplayStorePolicy {
        self.policy
    }

    fn load(&self, session_id: &str) -> Result<Vec<ReplayStoreBlob>, ReplayStoreError> {
        let replay_dir = self.replay_dir(session_id);
        if !replay_dir.is_dir() {
            return Ok(Vec::new());
        }
        let _lock = self.acquire_lock(&replay_dir)?;
        Self::cleanup_temp_files(&replay_dir)?;
        let mut blobs = Vec::new();
        let mut total = 0_u64;
        for path in Self::record_paths(&replay_dir)? {
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(decode_component)
                .ok_or_else(|| ReplayStoreError::new(ReplayStoreErrorCode::Corrupted))?;
            let mut file = File::open(&path).map_err(|_| store_io())?;
            let file_len = file.metadata().map_err(|_| store_io())?.len();
            if file_len > self.policy.max_session_bytes {
                return Err(ReplayStoreError::new(ReplayStoreErrorCode::Corrupted));
            }
            total = total.saturating_add(file_len);
            if total > self.policy.max_session_bytes {
                return Err(ReplayStoreError::new(ReplayStoreErrorCode::QuotaExceeded));
            }
            let mut payload = Vec::with_capacity(usize::try_from(file_len).unwrap_or(0));
            file.read_to_end(&mut payload).map_err(|_| store_io())?;
            blobs.push(ReplayStoreBlob::new(stem, payload));
        }
        Ok(blobs)
    }

    fn commit(&self, session_id: &str, blob: ReplayStoreBlob) -> Result<(), ReplayStoreError> {
        if len_u64(blob.payload.len()) > self.policy.max_session_bytes {
            return Err(ReplayStoreError::new(ReplayStoreErrorCode::QuotaExceeded));
        }
        let replay_dir = self.prepare_replay_dir(session_id)?;
        let _lock = self.acquire_lock(&replay_dir)?;
        Self::cleanup_temp_files(&replay_dir)?;
        let final_path =
            replay_dir.join(format!("{}{}", encode_component(&blob.id), RECORD_SUFFIX));
        if final_path.exists() {
            let existing = fs::read(&final_path).map_err(|_| store_io())?;
            return if existing == blob.payload {
                Ok(())
            } else {
                Err(ReplayStoreError::new(ReplayStoreErrorCode::Conflict))
            };
        }
        let total =
            Self::record_paths(&replay_dir)?
                .into_iter()
                .try_fold(0_u64, |total, path| {
                    path.metadata()
                        .map(|metadata| total.saturating_add(metadata.len()))
                        .map_err(|_| store_io())
                })?;
        if total.saturating_add(len_u64(blob.payload.len())) > self.policy.max_session_bytes {
            return Err(ReplayStoreError::new(ReplayStoreErrorCode::QuotaExceeded));
        }
        let temp_path = replay_dir.join(format!(
            ".{}.{}.{}.tmp",
            encode_component(&blob.id),
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut temp = secure_open(&temp_path, true)?;
            temp.write_all(&blob.payload).map_err(|_| store_io())?;
            temp.sync_all().map_err(|_| store_io())?;
            fs::rename(&temp_path, &final_path).map_err(|_| store_io())?;
            sync_directory(&replay_dir)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }

    fn remove(&self, session_id: &str, generation_ids: &[String]) -> Result<(), ReplayStoreError> {
        let replay_dir = self.replay_dir(session_id);
        if !replay_dir.is_dir() || generation_ids.is_empty() {
            return Ok(());
        }
        let _lock = self.acquire_lock(&replay_dir)?;
        for id in generation_ids {
            let path = replay_dir.join(format!("{}{}", encode_component(id), RECORD_SUFFIX));
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(store_io()),
            }
        }
        sync_directory(&replay_dir)
    }

    fn delete_session(&self, session_id: &str) -> Result<(), ReplayStoreError> {
        let replay_dir = self.replay_dir(session_id);
        if !replay_dir.exists() {
            return Ok(());
        }
        let _lock = self.acquire_lock(&replay_dir)?;
        Self::cleanup_temp_files(&replay_dir)?;
        for path in Self::record_paths(&replay_dir)? {
            fs::remove_file(path).map_err(|_| store_io())?;
        }
        sync_directory(&replay_dir)
    }
}

#[derive(Clone)]
pub(crate) struct CapturedItem {
    pub index: u32,
    pub content: CapturedContent,
    pub replay_requirement: sdk::ReplayRequirement,
    pub replay_token: Option<sdk::ReplayToken>,
}

#[derive(Clone)]
pub(crate) enum CapturedContent {
    Text {
        text: String,
    },
    Reasoning {
        kind: sdk::ReasoningKind,
        text: Option<String>,
    },
    ToolCall {
        call_id: String,
    },
}

pub(crate) struct ReplayHistory {
    by_message: HashMap<usize, Vec<CapturedItem>>,
    continuation: Option<ContinuationCandidate>,
}

struct ContinuationCandidate {
    handle: sdk::ContinuationHandle,
    message_index: usize,
    generation_id: String,
    invalidation: ReplayStoreBlob,
}

impl ReplayHistory {
    pub fn empty() -> Self {
        Self {
            by_message: HashMap::new(),
            continuation: None,
        }
    }

    pub fn items_for(&self, message_index: usize) -> &[CapturedItem] {
        self.by_message
            .get(&message_index)
            .map_or(&[], Vec::as_slice)
    }

    pub fn continuation_after(&self, message_index: usize) -> Option<&sdk::ContinuationHandle> {
        self.continuation
            .as_ref()
            .filter(|candidate| candidate.message_index == message_index)
            .map(|candidate| &candidate.handle)
    }

    pub fn has_continuation(&self) -> bool {
        self.continuation.is_some()
    }
}

pub(crate) struct ReplayCoordinator {
    store: Arc<dyn ModelReplayStore>,
    transient: Mutex<Vec<StoredGeneration>>,
    invalidated_continuations: Mutex<HashSet<String>>,
}

impl ReplayCoordinator {
    pub fn new(store: Arc<dyn ModelReplayStore>) -> Self {
        Self {
            store,
            transient: Mutex::new(Vec::new()),
            invalidated_continuations: Mutex::new(HashSet::new()),
        }
    }

    pub fn load(
        &self,
        client: &sdk::PhiloClient,
        target: &sdk::CallTarget,
        request: &ModelCallSnapshot,
        allow_server_continuation: bool,
    ) -> Result<ReplayHistory, ModelError> {
        if !request.persist_replay {
            return Ok(ReplayHistory::empty());
        }
        let blobs = self
            .store
            .load(request.session_id.as_str())
            .map_err(store_model_error)?;
        let now = unix_seconds()?;
        let mut generations = Vec::new();
        let mut remove = Vec::new();
        for blob in blobs {
            let generation: StoredGeneration = serde_json::from_slice(blob.payload())
                .map_err(|_| ModelError::new("model replay sidecar is corrupted"))?;
            if ![GENERATION_SCHEMA_V1, GENERATION_SCHEMA_V2].contains(&generation.schema_version)
                || generation.generation_id != blob.id()
                || generation.session_id != request.session_id.as_str()
            {
                return Err(ModelError::new("model replay sidecar is corrupted"));
            }
            if generation.expires_at_unix_secs <= now {
                remove.push(generation.generation_id);
            } else if generation.session_revision <= request.session_revision.get() {
                generations.push(generation);
            }
        }
        generations.sort_by_key(|generation| {
            (
                generation.session_revision,
                generation.created_at_unix_secs,
                generation.model_call_index,
            )
        });

        let mut invalidated = self
            .invalidated_continuations
            .lock()
            .map_err(|_| ModelError::new("model continuation state is unavailable"))?
            .clone();
        for generation in &generations {
            if let Some(id) = &generation.invalidates_generation {
                invalidated.insert(id.clone());
            }
        }

        let mut used = generations
            .iter()
            .filter(|generation| generation.invalidates_generation.is_some())
            .map(|generation| generation.generation_id.clone())
            .collect::<HashSet<_>>();
        let mut by_message = HashMap::new();
        let mut continuation = None;
        for (message_index, message) in request.messages.iter().enumerate() {
            let prefix_digest =
                continuation_prefix_digest_from_history(&request.messages[..=message_index]);
            let Some(generation) = generations.iter().find(|generation| {
                generation.invalidates_generation.is_none()
                    && !invalidated.contains(&generation.generation_id)
                    && !used.contains(&generation.generation_id)
                    && generation.matches(message)
                    && (generation.schema_version == GENERATION_SCHEMA_V1
                        || generation.continuation_prefix_digest.as_ref() == prefix_digest.as_ref())
            }) else {
                continue;
            };
            used.insert(generation.generation_id.clone());
            if generation.schema_version == GENERATION_SCHEMA_V2 {
                // A matched V2 generation is the newest chain boundary for
                // this history position, even when it intentionally carries
                // no response ID.
                continuation = None;
            }
            if !generation.target_matches(target) {
                continue;
            }
            match generation.restore(client, target) {
                Ok(Some(items)) => {
                    by_message.insert(message_index, items);
                    if allow_server_continuation
                        && generation.schema_version == GENERATION_SCHEMA_V2
                        && let Some(response_id) = &generation.response_id
                    {
                        let handle = client
                            .continuation_handle(target, response_id.clone())
                            .map_err(|_| {
                                ModelError::new("stored model continuation handle is invalid")
                            })?;
                        continuation = Some(ContinuationCandidate {
                            handle,
                            message_index,
                            generation_id: generation.generation_id.clone(),
                            invalidation: generation.invalidation_blob()?,
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }

        // Some compatible Chat protocols expose visible reasoning but no
        // serializable SDK replay token. Preserve their prior same-process,
        // same-turn behavior without putting unsnapshotted state on disk.
        let mut transient = self
            .transient
            .lock()
            .map_err(|_| ModelError::new("transient model replay state is unavailable"))?;
        transient.retain(|generation| {
            generation.session_id != request.session_id.as_str()
                || generation.turn_id == request.turn_id.as_str()
        });
        let mut transient_used = HashSet::new();
        for (message_index, message) in request.messages.iter().enumerate() {
            if by_message.contains_key(&message_index) {
                continue;
            }
            let Some(generation) = transient.iter().find(|generation| {
                generation.session_id == request.session_id.as_str()
                    && generation.turn_id == request.turn_id.as_str()
                    && generation.target_matches(target)
                    && !transient_used.contains(&generation.generation_id)
                    && generation.matches(message)
            }) else {
                continue;
            };
            transient_used.insert(generation.generation_id.clone());
            if let Some(items) = generation.restore(client, target)? {
                by_message.insert(message_index, items);
            }
        }
        drop(transient);

        let grace = self.store.policy().orphan_grace.as_secs();
        for generation in &generations {
            if !used.contains(&generation.generation_id)
                && generation.created_at_unix_secs.saturating_add(grace) <= now
            {
                remove.push(generation.generation_id.clone());
            }
        }
        if !remove.is_empty() {
            self.store
                .remove(request.session_id.as_str(), &remove)
                .map_err(store_model_error)?;
        }
        Ok(ReplayHistory {
            by_message,
            continuation,
        })
    }

    pub fn invalidate_continuation(&self, session_id: &str, history: &ReplayHistory) {
        let Some(candidate) = &history.continuation else {
            return;
        };
        let newly_invalidated = self
            .invalidated_continuations
            .lock()
            .map(|mut invalidated| invalidated.insert(candidate.generation_id.clone()))
            .unwrap_or(false);
        if newly_invalidated
            && let Err(error) = self
                .store
                .commit(session_id, candidate.invalidation.clone())
        {
            tracing::warn!(
                code = ?error.code(),
                "model continuation invalidation could not be persisted"
            );
        }
    }

    pub fn commit(
        &self,
        client: &sdk::PhiloClient,
        target: &sdk::CallTarget,
        request: &ModelCallSnapshot,
        response_id: Option<String>,
        mut items: Vec<CapturedItem>,
    ) -> Result<(), ModelError> {
        if !request.persist_replay || items.is_empty() {
            return Ok(());
        }
        items.sort_by_key(|item| item.index);
        let continuation_prefix_digest =
            continuation_prefix_digest_from_response(&request.messages, &items);
        let has_required = items
            .iter()
            .any(|item| item.replay_requirement == sdk::ReplayRequirement::Required);
        let now = unix_seconds()?;
        let expires = now.saturating_add(self.store.policy().ttl.as_secs());
        let mut stored_items = Vec::with_capacity(items.len());
        let mut has_snapshot = false;
        for item in items {
            let snapshot = match item.replay_token.as_ref() {
                Some(token) => match client.snapshot_replay(token) {
                    Ok(snapshot) => {
                        has_snapshot = true;
                        Some(snapshot)
                    }
                    Err(_) if item.replay_requirement != sdk::ReplayRequirement::Required => {
                        tracing::warn!(
                            code = "optional_replay_snapshot_failed",
                            item_index = item.index,
                            "optional model replay item will use semantic reconstruction"
                        );
                        None
                    }
                    Err(_) => return Err(required_snapshot_error()),
                },
                None if item.replay_requirement == sdk::ReplayRequirement::Required => {
                    return Err(required_snapshot_error());
                }
                None => None,
            };
            stored_items.push(StoredItem::from_captured(item, snapshot)?);
        }
        let generation_id = format!(
            "{}:{}:{}",
            request.context_fingerprint,
            request.turn_id.as_str(),
            request.model_call_id.as_str()
        );
        let generation = StoredGeneration {
            schema_version: GENERATION_SCHEMA_V2,
            generation_id: generation_id.clone(),
            session_id: request.session_id.as_str().to_owned(),
            context_fingerprint: request.context_fingerprint.clone(),
            session_revision: request.session_revision.get(),
            turn_id: request.turn_id.as_str().to_owned(),
            model_call_id: request.model_call_id.as_str().to_owned(),
            model_call_index: request.model_call_index,
            provider: target.provider().as_str().to_owned(),
            protocol: target.protocol().as_str().to_owned(),
            model: target.model().as_str().to_owned(),
            response_id,
            continuation_prefix_digest,
            invalidates_generation: None,
            created_at_unix_secs: now,
            expires_at_unix_secs: expires,
            items: stored_items,
        };
        if !has_snapshot {
            self.transient
                .lock()
                .map_err(|_| ModelError::new("transient model replay state is unavailable"))?
                .push(generation);
            return Ok(());
        }
        let payload = serde_json::to_vec(&generation)
            .map_err(|_| ModelError::new("model replay snapshot serialization failed"))?;
        match self.store.commit(
            request.session_id.as_str(),
            ReplayStoreBlob::new(generation_id, payload),
        ) {
            Ok(()) => Ok(()),
            Err(error) if !has_required => {
                tracing::warn!(
                    code = ?error.code(),
                    "optional model replay generation was not persisted"
                );
                Ok(())
            }
            Err(_) => Err(ModelError::new(
                "required model replay generation could not be persisted",
            )),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGeneration {
    schema_version: u32,
    generation_id: String,
    session_id: String,
    context_fingerprint: String,
    session_revision: u64,
    turn_id: String,
    model_call_id: String,
    model_call_index: u32,
    provider: String,
    protocol: String,
    model: String,
    response_id: Option<String>,
    #[serde(default)]
    continuation_prefix_digest: Option<String>,
    #[serde(default)]
    invalidates_generation: Option<String>,
    created_at_unix_secs: u64,
    expires_at_unix_secs: u64,
    items: Vec<StoredItem>,
}

impl StoredGeneration {
    fn invalidation_blob(&self) -> Result<ReplayStoreBlob, ModelError> {
        let generation_id = format!("{}:continuation-invalid", self.generation_id);
        let tombstone = Self {
            schema_version: GENERATION_SCHEMA_V2,
            generation_id: generation_id.clone(),
            session_id: self.session_id.clone(),
            context_fingerprint: self.context_fingerprint.clone(),
            session_revision: self.session_revision,
            turn_id: self.turn_id.clone(),
            model_call_id: self.model_call_id.clone(),
            model_call_index: self.model_call_index,
            provider: self.provider.clone(),
            protocol: self.protocol.clone(),
            model: self.model.clone(),
            response_id: None,
            continuation_prefix_digest: None,
            invalidates_generation: Some(self.generation_id.clone()),
            created_at_unix_secs: self.created_at_unix_secs,
            expires_at_unix_secs: self.expires_at_unix_secs,
            items: Vec::new(),
        };
        let payload = serde_json::to_vec(&tombstone)
            .map_err(|_| ModelError::new("model continuation invalidation serialization failed"))?;
        Ok(ReplayStoreBlob::new(generation_id, payload))
    }

    fn target_matches(&self, target: &sdk::CallTarget) -> bool {
        self.provider == target.provider().as_str()
            && self.protocol == target.protocol().as_str()
            && self.model == target.model().as_str()
    }

    fn matches(&self, message: &ModelMessage) -> bool {
        match message {
            ModelMessage::Assistant { content } => self.items.iter().any(|item| {
                matches!(
                    &item.content,
                    StoredContent::Text { digest } if digest == &text_digest(content)
                )
            }),
            ModelMessage::AssistantToolCalls { calls } => {
                let stored = self
                    .items
                    .iter()
                    .filter_map(|item| match &item.content {
                        StoredContent::ToolCall { call_id } => Some(call_id.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                stored.len() == calls.len()
                    && stored
                        .iter()
                        .zip(calls)
                        .all(|(stored, call)| *stored == call.tool_call_id.as_str())
            }
            _ => false,
        }
    }

    fn restore(
        &self,
        client: &sdk::PhiloClient,
        target: &sdk::CallTarget,
    ) -> Result<Option<Vec<CapturedItem>>, ModelError> {
        let mut restored = Vec::with_capacity(self.items.len());
        for item in &self.items {
            let token = match &item.snapshot {
                Some(snapshot) => match client.restore_replay(target, snapshot) {
                    Ok(token) => Some(token),
                    Err(error)
                        if matches!(
                            error.details(),
                            sdk::ErrorDetails::Replay {
                                reason: sdk::ReplayFailure::TargetMismatch,
                                ..
                            }
                        ) =>
                    {
                        return Ok(None);
                    }
                    Err(_) if item.requirement != StoredRequirement::Required => {
                        tracing::warn!(
                            code = "optional_replay_restore_failed",
                            item_index = item.index,
                            "optional model replay item will use semantic reconstruction"
                        );
                        None
                    }
                    Err(_) => return Err(required_restore_error()),
                },
                None if item.requirement == StoredRequirement::Required => {
                    return Err(required_restore_error());
                }
                None => None,
            };
            restored.push(item.to_captured(token)?);
        }
        restored.sort_by_key(|item| item.index);
        Ok(Some(restored))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredItem {
    index: u32,
    requirement: StoredRequirement,
    content: StoredContent,
    snapshot: Option<sdk::ReplaySnapshot>,
}

impl StoredItem {
    fn from_captured(
        item: CapturedItem,
        snapshot: Option<sdk::ReplaySnapshot>,
    ) -> Result<Self, ModelError> {
        let content = match item.content {
            CapturedContent::Text { text } => StoredContent::Text {
                digest: text_digest(&text),
            },
            CapturedContent::Reasoning { kind, text } => StoredContent::Reasoning {
                reasoning_kind: StoredReasoningKind::from_sdk(kind)?,
                text,
            },
            CapturedContent::ToolCall { call_id } => StoredContent::ToolCall { call_id },
        };
        Ok(Self {
            index: item.index,
            requirement: StoredRequirement::from_sdk(item.replay_requirement)?,
            content,
            snapshot,
        })
    }

    fn to_captured(
        &self,
        replay_token: Option<sdk::ReplayToken>,
    ) -> Result<CapturedItem, ModelError> {
        let content = match &self.content {
            StoredContent::Reasoning {
                reasoning_kind,
                text,
            } => CapturedContent::Reasoning {
                kind: reasoning_kind.to_sdk(),
                text: text.clone(),
            },
            StoredContent::Text { .. } => CapturedContent::Text {
                text: String::new(),
            },
            StoredContent::ToolCall { call_id } => CapturedContent::ToolCall {
                call_id: call_id.clone(),
            },
        };
        Ok(CapturedItem {
            index: self.index,
            content,
            replay_requirement: self.requirement.to_sdk(),
            replay_token,
        })
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StoredRequirement {
    None,
    Optional,
    Required,
}

impl StoredRequirement {
    fn from_sdk(value: sdk::ReplayRequirement) -> Result<Self, ModelError> {
        match value {
            sdk::ReplayRequirement::None => Ok(Self::None),
            sdk::ReplayRequirement::Optional => Ok(Self::Optional),
            sdk::ReplayRequirement::Required => Ok(Self::Required),
            _ => Err(ModelError::new("unsupported replay requirement")),
        }
    }

    const fn to_sdk(self) -> sdk::ReplayRequirement {
        match self {
            Self::None => sdk::ReplayRequirement::None,
            Self::Optional => sdk::ReplayRequirement::Optional,
            Self::Required => sdk::ReplayRequirement::Required,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredContent {
    Text {
        digest: String,
    },
    Reasoning {
        reasoning_kind: StoredReasoningKind,
        text: Option<String>,
    },
    ToolCall {
        call_id: String,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredReasoningKind {
    Summary,
    Exposed,
    Opaque,
}

impl StoredReasoningKind {
    fn from_sdk(value: sdk::ReasoningKind) -> Result<Self, ModelError> {
        match value {
            sdk::ReasoningKind::Summary => Ok(Self::Summary),
            sdk::ReasoningKind::Exposed => Ok(Self::Exposed),
            sdk::ReasoningKind::Opaque => Ok(Self::Opaque),
            _ => Err(ModelError::new("unsupported reasoning replay kind")),
        }
    }

    const fn to_sdk(self) -> sdk::ReasoningKind {
        match self {
            Self::Summary => sdk::ReasoningKind::Summary,
            Self::Exposed => sdk::ReasoningKind::Exposed,
            Self::Opaque => sdk::ReasoningKind::Opaque,
        }
    }
}

fn text_digest(text: &str) -> String {
    encode_digest(Sha256::digest(text.as_bytes()))
}

fn continuation_prefix_digest_from_response(
    messages: &[ModelMessage],
    items: &[CapturedItem],
) -> Option<String> {
    let mut hasher = Sha256::new();
    for message in messages {
        hash_message(&mut hasher, message);
    }
    let tool_calls = items
        .iter()
        .filter_map(|item| match &item.content {
            CapturedContent::ToolCall { call_id } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !tool_calls.is_empty() {
        hash_tag(&mut hasher, 5);
        for call_id in tool_calls {
            hash_str(&mut hasher, call_id);
        }
        return Some(encode_digest(hasher.finalize()));
    }

    let text = items
        .iter()
        .filter_map(|item| match &item.content {
            CapturedContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if text.is_empty() {
        return None;
    }
    hash_tag(&mut hasher, 4);
    hash_str(&mut hasher, &text);
    Some(encode_digest(hasher.finalize()))
}

fn continuation_prefix_digest_from_history(messages: &[ModelMessage]) -> Option<String> {
    let (terminal, prefix) = messages.split_last()?;
    let mut hasher = Sha256::new();
    for message in prefix {
        hash_message(&mut hasher, message);
    }
    match terminal {
        ModelMessage::Assistant { content } => {
            hash_tag(&mut hasher, 4);
            hash_str(&mut hasher, content);
        }
        ModelMessage::AssistantToolCalls { calls } => {
            hash_tag(&mut hasher, 5);
            for call in calls {
                hash_str(&mut hasher, call.tool_call_id.as_str());
            }
        }
        _ => return None,
    }
    Some(encode_digest(hasher.finalize()))
}

fn hash_message(hasher: &mut Sha256, message: &ModelMessage) {
    match message {
        ModelMessage::System { content } => {
            hash_tag(hasher, 1);
            hash_str(hasher, content);
        }
        ModelMessage::Summary { text } => {
            hash_tag(hasher, 2);
            hash_str(hasher, text);
        }
        ModelMessage::User { parts } => {
            hash_tag(hasher, 3);
            for part in parts {
                match part {
                    UserPart::Text(text) => {
                        hash_tag(hasher, 1);
                        hash_str(hasher, text);
                    }
                    UserPart::Image { media_type, bytes } => {
                        hash_tag(hasher, 2);
                        hash_str(hasher, media_type);
                        hash_bytes(hasher, bytes);
                    }
                }
            }
        }
        ModelMessage::Assistant { content } => {
            hash_tag(hasher, 4);
            hash_str(hasher, content);
        }
        ModelMessage::AssistantToolCalls { calls } => {
            hash_tag(hasher, 5);
            for call in calls {
                hash_str(hasher, call.tool_call_id.as_str());
                hash_str(hasher, &call.name);
                hash_str(hasher, &call.arguments);
            }
        }
        ModelMessage::ToolResult {
            tool_call_id,
            outcome,
        } => {
            hash_tag(hasher, 6);
            hash_str(hasher, tool_call_id.as_str());
            match outcome {
                ModelToolResultOutcome::Success { content } => {
                    hash_tag(hasher, 1);
                    hash_str(hasher, content);
                }
                ModelToolResultOutcome::Error { code, message } => {
                    hash_tag(hasher, 2);
                    hash_str(hasher, code);
                    hash_str(hasher, message);
                }
                ModelToolResultOutcome::Cancelled => hash_tag(hasher, 3),
                ModelToolResultOutcome::Interrupted => hash_tag(hasher, 4),
            }
        }
    }
}

fn hash_tag(hasher: &mut Sha256, tag: u8) {
    hasher.update([tag]);
}

fn hash_str(hasher: &mut Sha256, value: &str) {
    hash_bytes(hasher, value.as_bytes());
}

fn hash_bytes(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

fn encode_digest(digest: impl AsRef<[u8]>) -> String {
    let digest = digest.as_ref();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn unix_seconds() -> Result<u64, ModelError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| ModelError::new("system clock is before the Unix epoch"))
}

fn required_snapshot_error() -> ModelError {
    ModelError::new("required model replay snapshot is unavailable")
}

fn required_restore_error() -> ModelError {
    ModelError::new("required model replay snapshot could not be restored")
}

fn store_model_error(error: ReplayStoreError) -> ModelError {
    ModelError::new(error.to_string())
}

fn store_io() -> ReplayStoreError {
    ReplayStoreError::new(ReplayStoreErrorCode::Io)
}

fn len_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn session_dir_name(session_id: &str) -> String {
    format!("s-{}", encode_component(session_id))
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => encoded.push(char::from(byte)),
            other => {
                use std::fmt::Write as _;
                write!(&mut encoded, "%{other:02X}").expect("writing to String cannot fail");
            }
        }
    }
    encoded
}

fn decode_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = hex_value(*bytes.get(index + 1)?)?;
                let low = hex_value(*bytes.get(index + 2)?)?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte @ (b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_') => {
                decoded.push(byte);
                index += 1;
            }
            _ => return None,
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    (encode_component(&decoded) == value).then_some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(unix)]
fn secure_open(path: impl AsRef<Path>, create_new: bool) -> Result<File, ReplayStoreError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .create(!create_new)
        .create_new(create_new)
        .read(!create_new)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|_| store_io())
}

#[cfg(not(unix))]
fn secure_open(path: impl AsRef<Path>, create_new: bool) -> Result<File, ReplayStoreError> {
    OpenOptions::new()
        .create(!create_new)
        .create_new(create_new)
        .read(!create_new)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|_| store_io())
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<(), ReplayStoreError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| store_io())
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<(), ReplayStoreError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ReplayStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| store_io())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ReplayStoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_v1_deserializes_without_continuation_metadata() {
        let generation: StoredGeneration = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generation_id": "generation-v1",
            "session_id": "session",
            "context_fingerprint": "context",
            "session_revision": 0,
            "turn_id": "turn",
            "model_call_id": "call",
            "model_call_index": 1,
            "provider": "provider",
            "protocol": "openai-responses/openai-v2",
            "model": "model",
            "response_id": null,
            "created_at_unix_secs": 1,
            "expires_at_unix_secs": 2,
            "items": []
        }))
        .expect("V1 replay generation remains readable");

        assert_eq!(generation.schema_version, GENERATION_SCHEMA_V1);
        assert!(generation.continuation_prefix_digest.is_none());
        assert!(generation.invalidates_generation.is_none());
    }
}
