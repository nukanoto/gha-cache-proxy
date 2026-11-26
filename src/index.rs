//! Cache metadata and reservation state, persisted as one JSON object in S3.
//! The in-memory copy is the source of truth for lookups; every mutation is
//! serialized through the process lock, built on a clone, persisted, and only
//! then swapped into the in-memory index, so a failed save never leaves
//! changed state behind.

use serde::{Deserialize, Serialize};

use crate::store::{S3Result, Store};
use crate::{unix_now, AppState};

/// Reservations older than this are reclaimable when a new save with the same
/// key/version arrives (a dead job would otherwise block the key forever).
pub const RESERVATION_TTL_SECS: u64 = 24 * 3600;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub key: String,
    pub version: String,
    pub size: u64,
    pub created_at: u64,
    /// Last access timestamp for LRU eviction. Defaulted so indices written
    /// before this field still load cleanly (falling back to `created_at`).
    #[serde(default)]
    pub last_accessed_at: u64,
}

impl Entry {
    pub fn effective_last_accessed(&self) -> u64 {
        self.last_accessed_at.max(self.created_at)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Reservation {
    pub id: u64,
    pub key: String,
    pub version: String,
    pub created_at: u64,
    /// "v1" (PATCH chunks) or "v2" (Azure blob staging).
    pub protocol: String,
    /// Aggregate bytes staged under this reservation (v1 chunks + v2 blocks),
    /// bounded by the configured maximum; replacing a part counts its new
    /// size only. Defaulted so indices written before this field still load.
    #[serde(default)]
    pub staged_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Index {
    pub next_id: u64,
    pub entries: Vec<Entry>,
    pub reservations: Vec<Reservation>,
}

impl Index {
    /// Loaded exactly once at startup; a missing object means a fresh store.
    pub async fn load(store: &Store) -> S3Result<Self> {
        match store.get_bytes(&store.index_key()).await? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("corrupt cache index: {e}").into()),
            None => Ok(Self::default()),
        }
    }

    pub async fn save(&self, store: &Store) -> S3Result<()> {
        store
            .put_bytes(&store.index_key(), serde_json::to_vec(self)?)
            .await
    }

    pub fn is_stale(&self, r: &Reservation, now: u64) -> bool {
        now.saturating_sub(r.created_at) >= RESERVATION_TTL_SECS
    }

    pub fn find_entry(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn find_entry_by_key(&self, key: &str, version: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.key == key && e.version == version)
    }

    pub fn find_reservation(&self, id: u64) -> Option<&Reservation> {
        self.reservations.iter().find(|r| r.id == id)
    }

    /// Update `last_accessed_at` in memory on a cache hit (persisted on the
    /// next index save).
    pub fn touch_entry(&mut self, id: u64, now: u64) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            if now > entry.last_accessed_at {
                entry.last_accessed_at = now;
            }
        }
    }
}

/// Evict least-recently-used entries until the aggregate entry size is at most
/// `max_total_bytes` (0 disables eviction). Never evicts `protected_id` so the
/// entry just committed is always preserved. Returns the evicted entry IDs.
pub fn evict_lru(entries: &mut Vec<Entry>, max_total_bytes: u64, protected_id: u64) -> Vec<u64> {
    if max_total_bytes == 0 {
        return Vec::new();
    }
    let mut total: u64 = entries
        .iter()
        .map(|e| e.size)
        .fold(0u64, u64::saturating_add);
    let mut evicted = Vec::new();
    while total > max_total_bytes {
        let victim_idx = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.id != protected_id)
            .min_by_key(|(_, e)| (e.effective_last_accessed(), e.created_at, e.id))
            .map(|(i, _)| i);
        let Some(idx) = victim_idx else {
            break;
        };
        let victim = entries.swap_remove(idx);
        total = total.saturating_sub(victim.size);
        evicted.push(victim.id);
    }
    evicted
}

/// GitHub matching order: exact primary key first, then each restore key as a
/// prefix in the given order; among matches the most recently created wins.
pub fn lookup<'a>(
    entries: &'a [Entry],
    primary: &str,
    restore_keys: &[String],
    version: &str,
) -> Option<&'a Entry> {
    let exact = entries
        .iter()
        .filter(|e| e.key == primary && e.version == version)
        .max_by_key(|e| (e.created_at, e.id));
    if exact.is_some() {
        return exact;
    }
    for rk in restore_keys {
        let hit = entries
            .iter()
            .filter(|e| e.version == version && e.key.starts_with(rk.as_str()))
            .max_by_key(|e| (e.created_at, e.id));
        if hit.is_some() {
            return hit;
        }
    }
    None
}

/// JavaScript `String.length` counts UTF-16 code units, not bytes; the cache
/// key rules of @actions/cache use that length, so mirror it here.
pub fn utf16_len(s: &str) -> usize {
    s.chars()
        .map(|c| if (c as u32) <= 0xFFFF { 1 } else { 2 })
        .sum()
}

/// Mirror the client-side rules: non-empty, at most 512 chars, no commas.
pub fn validate_key(key: &str) -> bool {
    !key.is_empty() && utf16_len(key) <= 512 && !key.contains(',')
}

pub fn validate_version(version: &str) -> bool {
    !version.is_empty() && utf16_len(version) <= 512
}

/// True when `chunks` (offset, size) sorted by offset tile [0, total) exactly.
pub fn validate_coverage(chunks: &[(u64, u64)], total: u64) -> bool {
    let mut pos = 0u64;
    for (off, size) in chunks {
        if *off != pos || *size == 0 {
            return false;
        }
        match pos.checked_add(*size) {
            Some(next) if next <= total => pos = next,
            _ => return false,
        }
    }
    pos == total
}

#[derive(Debug, PartialEq)]
pub enum ReserveError {
    Conflict,
    TooLarge,
    Internal,
}

/// Create a reservation for a new upload. A completed entry with the same
/// key/version, or a fresh reservation, is a conflict; a stale reservation is
/// reclaimed (its staging objects are deleted asynchronously) only after the
/// new reservation persisted successfully.
pub async fn reserve(
    state: &AppState,
    key: &str,
    version: &str,
    protocol: &str,
    cache_size: Option<u64>,
) -> Result<u64, ReserveError> {
    let mut idx = state.index.lock().await;
    if cache_size.is_some_and(|s| s > state.cfg.max_cache_size_bytes) {
        return Err(ReserveError::TooLarge);
    }
    let now = unix_now();
    if idx.find_entry_by_key(key, version).is_some() {
        return Err(ReserveError::Conflict);
    }
    let mut next = idx.clone();
    let mut reclaimed: Option<u64> = None;
    if let Some(existing) = next
        .reservations
        .iter()
        .find(|r| r.key == key && r.version == version)
        .cloned()
    {
        if !next.is_stale(&existing, now) {
            return Err(ReserveError::Conflict);
        }
        next.reservations.retain(|r| r.id != existing.id);
        reclaimed = Some(existing.id);
    }
    let id = next.next_id;
    next.next_id += 1;
    next.reservations.push(Reservation {
        id,
        key: key.to_string(),
        version: version.to_string(),
        created_at: now,
        protocol: protocol.to_string(),
        staged_bytes: 0,
    });
    next.save(&state.store).await.map_err(|e| {
        tracing::error!("failed to persist cache index: {e}");
        ReserveError::Internal
    })?;
    *idx = next;
    if let Some(stale_id) = reclaimed {
        let store = state.store.clone();
        // Reclaim outside the lock; a fresh id keeps the new upload isolated.
        tokio::spawn(async move {
            if let Err(e) = store.delete_prefix(&store.staging_prefix(stale_id)).await {
                tracing::warn!("stale reservation cleanup failed for {stale_id}: {e}");
            }
        });
    }
    Ok(id)
}

/// Outcome of a commit/finalize attempt that finds its reservation gone: a
/// concurrent retry may have completed the entry already.
#[derive(Debug, PartialEq)]
pub enum CompletionOutcome {
    /// A completed entry with the same id/size exists: idempotent success.
    AlreadyCompleted,
    /// A completed entry exists but declares a different size.
    SizeConflict,
    /// No entry references the archive: it is orphaned and may be deleted.
    Orphan,
}

/// A losing commit/finalize must never delete an archive a completed entry
/// references; only an Orphan may be cleaned up.
pub fn completion_outcome(idx: &Index, id: u64, size: u64) -> CompletionOutcome {
    match idx.find_entry(id) {
        Some(e) if e.size == size => CompletionOutcome::AlreadyCompleted,
        Some(_) => CompletionOutcome::SizeConflict,
        None => CompletionOutcome::Orphan,
    }
}

/// Aggregate for replacing a staged part of `old_size` with one of `new_size`.
/// Saturating keeps the budget from wrapping when accounting races; the caller
/// rejects the result when it exceeds the configured maximum.
pub fn apply_staged_delta(staged: u64, old_size: u64, new_size: u64) -> u64 {
    match new_size.checked_sub(old_size) {
        Some(delta) => staged.saturating_add(delta),
        None => staged.saturating_sub(old_size - new_size),
    }
}

#[derive(Debug, PartialEq)]
pub enum StagingError {
    NoReservation,
    OverBudget,
    Internal,
}

/// Reserve `new_size` bytes of the reservation's staging budget for the part
/// at `part_key`, replacing any previously staged part of the same key. The
/// delta is persisted before it is applied to the in-memory index; callers
/// must roll it back with `release_staging` when the upload fails.
pub async fn reserve_staging(
    state: &AppState,
    id: u64,
    part_key: &str,
    new_size: u64,
) -> Result<u64, StagingError> {
    let mut idx = state.index.lock().await;
    let Some(reservation) = idx.find_reservation(id) else {
        return Err(StagingError::NoReservation);
    };
    // The old part size comes from S3. Reading it under the index lock keeps
    // the accounting exact: a completed replacement has already committed
    // both its S3 object and its aggregate before this check runs. A HEAD
    // failure counts the full new size (fail closed) instead of guessing.
    let old_size = match state.store.head(part_key).await {
        Ok(Some(h)) => h.content_length().map(|l| l.max(0) as u64).unwrap_or(0),
        Ok(None) => 0,
        Err(e) => {
            tracing::warn!("failed to inspect staged part {part_key}: {e}");
            0
        }
    };
    let aggregate = apply_staged_delta(reservation.staged_bytes, old_size, new_size);
    if aggregate > state.cfg.max_cache_size_bytes {
        return Err(StagingError::OverBudget);
    }
    let mut next = idx.clone();
    let reservation = next
        .reservations
        .iter_mut()
        .find(|r| r.id == id)
        .expect("reservation verified above");
    reservation.staged_bytes = aggregate;
    next.save(&state.store).await.map_err(|e| {
        tracing::error!("failed to persist cache index: {e}");
        StagingError::Internal
    })?;
    *idx = next;
    Ok(old_size)
}

/// Roll back the delta reserved by `reserve_staging` after a failed upload.
/// No-op when the reservation no longer exists (completed or reclaimed).
pub async fn release_staging(
    state: &AppState,
    id: u64,
    old_size: u64,
    new_size: u64,
) -> S3Result<()> {
    let mut idx = state.index.lock().await;
    let Some(_) = idx.find_reservation(id) else {
        return Ok(());
    };
    let mut next = idx.clone();
    let reservation = next
        .reservations
        .iter_mut()
        .find(|r| r.id == id)
        .expect("reservation verified above");
    reservation.staged_bytes = apply_staged_delta(reservation.staged_bytes, new_size, old_size);
    next.save(&state.store).await?;
    *idx = next;
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum CommitError {
    NotFound,
    BadUpload,
    TooLarge,
    Internal,
}

/// v1 commit: assemble the PATCH chunks into the final archive and mark the
/// entry completed. The archive is only referenced by the index after the
/// reservation is confirmed still alive, so a concurrently reclaimed
/// reservation can never produce a phantom entry. A retried commit of an
/// already-completed entry with the same size is idempotent success.
pub async fn commit_v1(state: &AppState, id: u64, size: u64) -> Result<(), CommitError> {
    if size > state.cfg.max_cache_size_bytes {
        return Err(CommitError::TooLarge);
    }
    {
        let idx = state.index.lock().await;
        match completion_outcome(&idx, id, size) {
            CompletionOutcome::AlreadyCompleted => return Ok(()),
            CompletionOutcome::SizeConflict => return Err(CommitError::NotFound),
            CompletionOutcome::Orphan => {}
        }
    }
    let reservation = {
        let idx = state.index.lock().await;
        idx.find_reservation(id)
            .filter(|r| r.protocol == "v1")
            .cloned()
    };
    let Some(reservation) = reservation else {
        return Err(CommitError::NotFound);
    };

    let objects = state
        .store
        .list(&state.store.chunk_prefix(id))
        .await
        .map_err(|e| {
            tracing::error!("failed to list upload chunks: {e}");
            CommitError::Internal
        })?;
    let mut chunks: Vec<(u64, u64)> = objects
        .iter()
        .filter_map(|o| {
            let key = o.key.as_ref()?;
            let offset: u64 = key.rsplit('/').next()?.parse().ok()?;
            Some((offset, o.size.unwrap_or(0) as u64))
        })
        .collect();
    chunks.sort_unstable();
    if !validate_coverage(&chunks, size) {
        return Err(CommitError::BadUpload);
    }
    let parts: Vec<(String, u64)> = chunks
        .iter()
        .map(|(off, sz)| (state.store.chunk_key(id, *off), *sz))
        .collect();
    state
        .store
        .concat(&state.store.archive_key(id), size, parts)
        .await
        .map_err(|e| {
            tracing::error!("failed to assemble archive: {e}");
            CommitError::Internal
        })?;

    let mut idx = state.index.lock().await;
    let Some(_) = idx.find_reservation(id) else {
        // The reservation was reclaimed or a concurrent commit won the race.
        // Never delete an archive a completed entry references; a same-size
        // retry is idempotent success.
        return match completion_outcome(&idx, id, size) {
            CompletionOutcome::AlreadyCompleted => Ok(()),
            CompletionOutcome::SizeConflict => Err(CommitError::NotFound),
            CompletionOutcome::Orphan => {
                let store = state.store.clone();
                tokio::spawn(async move {
                    let _ = store.delete_many(vec![store.archive_key(id)]).await;
                });
                Err(CommitError::NotFound)
            }
        };
    };
    let mut next = idx.clone();
    let now = unix_now();
    next.reservations.retain(|r| r.id != id);
    next.entries.push(Entry {
        id,
        key: reservation.key,
        version: reservation.version,
        size,
        created_at: now,
        last_accessed_at: now,
    });
    let evicted = evict_lru(&mut next.entries, state.cfg.max_total_cache_size_bytes, id);
    next.save(&state.store).await.map_err(|e| {
        tracing::error!("failed to persist cache index: {e}");
        CommitError::Internal
    })?;
    *idx = next;
    drop(idx);

    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = store.delete_prefix(&store.chunk_prefix(id)).await {
            tracing::warn!("chunk cleanup failed for {id}: {e}");
        }
        if !evicted.is_empty() {
            tracing::info!(count = evicted.len(), "evicted LRU cache entries");
            let keys: Vec<String> = evicted.into_iter().map(|eid| store.archive_key(eid)).collect();
            if let Err(e) = store.delete_many(keys).await {
                tracing::warn!("LRU archive cleanup failed: {e}");
            }
        }
    });
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum FinalizeError {
    NoReservation,
    NoUpload,
    SizeMismatch,
    Internal,
}

/// v2 finalize: verify the staged blob, copy it to the archive slot and mark
/// the entry completed. The request carries no reservation id, so the active
/// reservation is resolved by key/version. Retries are idempotent: an
/// already-completed entry with the same key/version answers ok with its id.
pub async fn finalize_v2(
    state: &AppState,
    key: &str,
    version: &str,
    size_bytes: u64,
) -> Result<u64, FinalizeError> {
    {
        let idx = state.index.lock().await;
        if let Some(entry) = idx.find_entry_by_key(key, version) {
            return if entry.size == size_bytes {
                Ok(entry.id)
            } else {
                Err(FinalizeError::SizeMismatch)
            };
        }
    }
    let reservation = {
        let idx = state.index.lock().await;
        idx.reservations
            .iter()
            .find(|r| r.protocol == "v2" && r.key == key && r.version == version)
            .cloned()
    };
    let Some(reservation) = reservation else {
        return Err(FinalizeError::NoReservation);
    };
    let id = reservation.id;
    if size_bytes > state.cfg.max_cache_size_bytes {
        return Err(FinalizeError::SizeMismatch);
    }
    let staging = state.store.staging_blob_key(id);
    let staged = state.store.head(&staging).await.map_err(|e| {
        tracing::error!("failed to inspect staged blob: {e}");
        FinalizeError::Internal
    })?;
    let Some(staged) = staged else {
        return Err(FinalizeError::NoUpload);
    };
    if staged.content_length() != Some(size_bytes as i64) {
        return Err(FinalizeError::SizeMismatch);
    }
    let archive = state.store.archive_key(id);
    state.store.copy(&staging, &archive).await.map_err(|e| {
        tracing::error!("failed to move staged blob into place: {e}");
        FinalizeError::Internal
    })?;

    let mut idx = state.index.lock().await;
    let Some(_) = idx.find_reservation(id) else {
        return match completion_outcome(&idx, id, size_bytes) {
            CompletionOutcome::AlreadyCompleted => Ok(id),
            CompletionOutcome::SizeConflict => Err(FinalizeError::SizeMismatch),
            CompletionOutcome::Orphan => {
                let store = state.store.clone();
                tokio::spawn(async move {
                    let _ = store.delete_many(vec![store.archive_key(id)]).await;
                });
                Err(FinalizeError::NoReservation)
            }
        };
    };
    let mut next = idx.clone();
    let now = unix_now();
    next.reservations.retain(|r| r.id != id);
    next.entries.push(Entry {
        id,
        key: key.to_string(),
        version: version.to_string(),
        size: size_bytes,
        created_at: now,
        last_accessed_at: now,
    });
    let evicted = evict_lru(&mut next.entries, state.cfg.max_total_cache_size_bytes, id);
    next.save(&state.store).await.map_err(|e| {
        tracing::error!("failed to persist cache index: {e}");
        FinalizeError::Internal
    })?;
    *idx = next;
    drop(idx);

    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = store.delete_prefix(&store.staging_prefix(id)).await {
            tracing::warn!("staging cleanup failed for {id}: {e}");
        }
        if !evicted.is_empty() {
            tracing::info!(count = evicted.len(), "evicted LRU cache entries");
            let keys: Vec<String> = evicted.into_iter().map(|eid| store.archive_key(eid)).collect();
            if let Err(e) = store.delete_many(keys).await {
                tracing::warn!("LRU archive cleanup failed: {e}");
            }
        }
    });
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, key: &str, version: &str, created_at: u64) -> Entry {
        Entry {
            id,
            key: key.to_string(),
            version: version.to_string(),
            size: 10,
            created_at,
            last_accessed_at: created_at,
        }
    }

    #[test]
    fn evict_lru_removes_oldest_accessed_and_protects_new_entry() {
        let mut entries = vec![
            Entry {
                id: 1,
                key: "k1".into(),
                version: "v1".into(),
                size: 40,
                created_at: 100,
                last_accessed_at: 500, // recently hit
            },
            Entry {
                id: 2,
                key: "k2".into(),
                version: "v1".into(),
                size: 40,
                created_at: 200,
                last_accessed_at: 200, // oldest effective access
            },
            Entry {
                id: 3,
                key: "k3".into(),
                version: "v1".into(),
                size: 40,
                created_at: 300,
                last_accessed_at: 300,
            },
        ];
        // Limit 80 bytes: total is 120, must evict id 2 (not id 1 which was touched at 500).
        let evicted = evict_lru(&mut entries, 80, 3);
        assert_eq!(evicted, vec![2]);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.id == 1));
        assert!(entries.iter().any(|e| e.id == 3));

        // Limit 10 bytes (< protected entry size 40): evicts id 1, keeps protected id 3.
        let evicted2 = evict_lru(&mut entries, 10, 3);
        assert_eq!(evicted2, vec![1]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 3);

        // Limit 0 disables eviction.
        let evicted3 = evict_lru(&mut entries, 0, 99);
        assert!(evicted3.is_empty());
        assert_eq!(entries.len(), 1);
    }

    fn reservation(id: u64, key: &str, version: &str, created_at: u64) -> Reservation {
        Reservation {
            id,
            key: key.to_string(),
            version: version.to_string(),
            created_at,
            protocol: "v1".into(),
            staged_bytes: 0,
        }
    }

    #[test]
    fn primary_key_returns_newest_exact_match() {
        println!("Verify that an exact primary-key match returns the newest entry");
        let entries = vec![
            entry(1, "linux-abc", "v1", 100),
            entry(2, "linux-abc", "v1", 200),
        ];
        let hit = lookup(&entries, "linux-abc", &[], "v1").expect("ヒットするはず");
        assert_eq!(hit.id, 2);
    }

    #[test]
    fn restore_keys_try_prefixes_in_order() {
        println!("Verify that restore-key prefixes are tried in order");
        let entries = vec![
            entry(1, "linux-main-b", "v1", 100),
            entry(2, "linux-main-a", "v1", 200),
        ];
        let hit = lookup(
            &entries,
            "no-such",
            &["linux-x".into(), "linux-main".into()],
            "v1",
        )
        .expect("2番目の復元キーでヒットするはず");
        assert_eq!(hit.id, 2); // Newest entry within the prefix
        let hit2 = lookup(
            &entries,
            "no-such",
            &["linux-main".into(), "linux-main-b".into()],
            "v1",
        )
        .expect("ヒットするはず");
        assert_eq!(hit2.id, 2);
    }

    #[test]
    fn version_or_restore_mismatch_is_a_miss() {
        println!("Verify that version and restore-key mismatches are misses");
        let entries = vec![entry(1, "linux-abc", "v1", 100)];
        assert!(lookup(&entries, "linux-abc", &[], "v2").is_none());
        assert!(lookup(&entries, "linux-abc", &[], "v1").is_some());
        assert!(lookup(&entries, "linux-ab", &["linux-ab".into()], "v1").is_some());
        assert!(lookup(&entries, "linux-x", &["linux-ay".into()], "v1").is_none());
    }

    #[test]
    fn coverage_rejects_gaps_overlaps_and_excess() {
        println!("Verify that chunk coverage rejects gaps, overlaps, and excess");
        assert!(validate_coverage(&[(0, 10)], 10));
        assert!(validate_coverage(&[(0, 4), (4, 6)], 10));
        assert!(!validate_coverage(&[(1, 4)], 4)); // Does not start at zero
        assert!(!validate_coverage(&[(0, 4), (5, 6)], 10)); // Has a gap
        assert!(!validate_coverage(&[(0, 4), (3, 6)], 10)); // Overlaps
        assert!(!validate_coverage(&[(0, 4)], 10)); // Is incomplete
        assert!(validate_coverage(&[(0, 4), (4, 4), (8, 4)], 12));
        assert!(!validate_coverage(&[(0, 4), (4, 0), (4, 8)], 12)); // Has a zero-size chunk
        assert!(validate_coverage(&[], 0)); // Empty upload is valid for zero bytes
    }

    #[test]
    fn key_and_version_validation_matches_the_client() {
        println!("Verify that key and version validation matches client constraints");
        assert!(validate_key("a".repeat(512).as_str()));
        assert!(!validate_key("a".repeat(513).as_str()));
        assert!(!validate_key("a,b"));
        assert!(!validate_key(""));
        assert!(validate_version("v"));
        assert!(!validate_version(""));
    }

    #[test]
    fn key_length_counts_utf16_code_units() {
        println!("Verify that key length uses UTF-16 code units like JavaScript");
        assert_eq!(utf16_len("a"), 1);
        assert_eq!(utf16_len("あ"), 1); // One code unit despite three UTF-8 bytes
        assert_eq!(utf16_len("😀"), 2); // A surrogate pair uses two code units
        assert_eq!(utf16_len("a😀あ"), 4);
        // More than 512 UTF-8 bytes can still be valid within 512 code units
        assert!(validate_key("あ".repeat(512).as_str()));
        assert!(!validate_key("あ".repeat(513).as_str()));
        assert!(validate_key("😀".repeat(256).as_str()));
        assert!(!validate_key("😀".repeat(257).as_str()));
        assert!(validate_version("😀".repeat(256).as_str()));
        assert!(!validate_version("😀".repeat(257).as_str()));
    }

    #[test]
    fn reservation_expires_after_ttl() {
        println!("Verify that a reservation expires after its TTL");
        let idx = Index {
            next_id: 1,
            entries: vec![],
            reservations: vec![reservation(1, "k", "v", 100)],
        };
        let r = idx.find_reservation(1).unwrap();
        assert!(!idx.is_stale(r, 100 + RESERVATION_TTL_SECS - 1));
        assert!(idx.is_stale(r, 100 + RESERVATION_TTL_SECS));
    }

    #[test]
    fn completion_distinguishes_idempotent_success_from_orphan() {
        println!("Verify that completion distinguishes idempotent success from an orphan archive");
        let idx = Index {
            next_id: 2,
            entries: vec![entry(7, "k", "v", 100)],
            reservations: vec![],
        };
        // The same size is an idempotent success for a completed entry
        assert_eq!(
            completion_outcome(&idx, 7, 10),
            CompletionOutcome::AlreadyCompleted
        );
        // A different size is a conflict
        assert_eq!(
            completion_outcome(&idx, 7, 99),
            CompletionOutcome::SizeConflict
        );
        // No entry means the archive is orphaned
        assert_eq!(completion_outcome(&idx, 8, 10), CompletionOutcome::Orphan);
    }

    #[test]
    fn staging_budget_accounts_for_replacement_delta() {
        println!("Verify that the staging budget accounts for replacement deltas");
        assert_eq!(apply_staged_delta(100, 30, 50), 120); // Replacement adds 20
        assert_eq!(apply_staged_delta(100, 30, 200), 270);
        assert_eq!(apply_staged_delta(100, 200, 50), 0); // Shrink saturates at zero
        assert_eq!(apply_staged_delta(0, 0, 10), 10); // New part
        assert_eq!(apply_staged_delta(10, 10, 10), 10); // Same-size replacement is unchanged
        assert_eq!(apply_staged_delta(u64::MAX, 0, u64::MAX), u64::MAX); // Addition saturates
    }

    #[test]
    fn legacy_index_reservation_loads_with_zero_budget() {
        println!("Verify that a legacy index reservation loads with zero budget");
        let json = r#"{"id":1,"key":"k","version":"v","created_at":100,"protocol":"v1"}"#;
        let r: Reservation = serde_json::from_str(json).unwrap();
        assert_eq!(r.staged_bytes, 0);
    }

    #[test]
    fn index_json_round_trip_preserves_contents() {
        println!("Verify that an index JSON round trip preserves its contents");
        let idx = Index {
            next_id: 3,
            entries: vec![entry(1, "k", "v", 100)],
            reservations: vec![Reservation {
                id: 2,
                key: "k2".into(),
                version: "v".into(),
                created_at: 200,
                protocol: "v1".into(),
                staged_bytes: 42,
            }],
        };
        let bytes = serde_json::to_vec(&idx).unwrap();
        let decoded: Index = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(idx, decoded);
    }
}
