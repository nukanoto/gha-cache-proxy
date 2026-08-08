//! Cache metadata and reservation state, persisted as one JSON object in S3.
//! The in-memory copy is the source of truth for lookups; every mutation is
//! serialized through the process lock and persisted before the lock is
//! released, which makes single-replica reservation semantics deterministic.

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
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Reservation {
    pub id: u64,
    pub key: String,
    pub version: String,
    pub created_at: u64,
    /// "v1" (PATCH chunks) or "v2" (Azure blob staging).
    pub protocol: String,
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

/// Mirror the client-side rules: non-empty, at most 512 chars, no commas.
pub fn validate_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 512 && !key.contains(',')
}

pub fn validate_version(version: &str) -> bool {
    !version.is_empty() && version.len() <= 512
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
/// reclaimed (its staging objects are deleted asynchronously).
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
    if let Some(existing) = idx
        .reservations
        .iter()
        .find(|r| r.key == key && r.version == version)
        .cloned()
    {
        if !idx.is_stale(&existing, now) {
            return Err(ReserveError::Conflict);
        }
        idx.reservations.retain(|r| r.id != existing.id);
        let store = state.store.clone();
        // Reclaim outside the lock; a fresh id keeps the new upload isolated.
        tokio::spawn(async move {
            if let Err(e) = store
                .delete_prefix(&store.staging_prefix(existing.id))
                .await
            {
                tracing::warn!("stale reservation cleanup failed for {}: {e}", existing.id);
            }
        });
    }
    let id = idx.next_id;
    idx.next_id += 1;
    idx.reservations.push(Reservation {
        id,
        key: key.to_string(),
        version: version.to_string(),
        created_at: now,
        protocol: protocol.to_string(),
    });
    idx.save(&state.store).await.map_err(|e| {
        tracing::error!("failed to persist cache index: {e}");
        ReserveError::Internal
    })?;
    Ok(id)
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
/// reservation can never produce a phantom entry.
pub async fn commit_v1(state: &AppState, id: u64, size: u64) -> Result<(), CommitError> {
    if size > state.cfg.max_cache_size_bytes {
        return Err(CommitError::TooLarge);
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
    if idx.find_reservation(id).is_none() {
        // The reservation was reclaimed while we assembled; discard the archive.
        let store = state.store.clone();
        tokio::spawn(async move {
            let _ = store.delete_many(vec![store.archive_key(id)]).await;
        });
        return Err(CommitError::NotFound);
    }
    idx.reservations.retain(|r| r.id != id);
    idx.entries.push(Entry {
        id,
        key: reservation.key,
        version: reservation.version,
        size,
        created_at: unix_now(),
    });
    idx.save(&state.store).await.map_err(|e| {
        tracing::error!("failed to persist cache index: {e}");
        CommitError::Internal
    })?;
    drop(idx);

    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = store.delete_prefix(&store.chunk_prefix(id)).await {
            tracing::warn!("chunk cleanup failed for {id}: {e}");
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
    if idx.find_reservation(id).is_none() {
        let store = state.store.clone();
        tokio::spawn(async move {
            let _ = store.delete_many(vec![store.archive_key(id)]).await;
        });
        return Err(FinalizeError::NoReservation);
    }
    idx.reservations.retain(|r| r.id != id);
    idx.entries.push(Entry {
        id,
        key: key.to_string(),
        version: version.to_string(),
        size: size_bytes,
        created_at: unix_now(),
    });
    idx.save(&state.store).await.map_err(|e| {
        tracing::error!("failed to persist cache index: {e}");
        FinalizeError::Internal
    })?;
    drop(idx);

    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = store.delete_prefix(&store.staging_prefix(id)).await {
            tracing::warn!("staging cleanup failed for {id}: {e}");
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
        }
    }

    #[test]
    fn プライマリキーは完全一致で最新が選ばれる() {
        println!("プライマリキー完全一致で作成日時が最新のエントリが返ることを確認");
        let entries = vec![
            entry(1, "linux-abc", "v1", 100),
            entry(2, "linux-abc", "v1", 200),
        ];
        let hit = lookup(&entries, "linux-abc", &[], "v1").expect("ヒットするはず");
        assert_eq!(hit.id, 2);
    }

    #[test]
    fn 復元キーはプレフィックス一致が順序どおり試される() {
        println!("復元キーのプレフィックス一致が指定順で試されることを確認");
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
        assert_eq!(hit.id, 2); // 同じプレフィックス内では最新
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
    fn バージョン不一致と復元キー不一致はミスになる() {
        println!("バージョン不一致や復元キー不一致ではミスになることを確認");
        let entries = vec![entry(1, "linux-abc", "v1", 100)];
        assert!(lookup(&entries, "linux-abc", &[], "v2").is_none());
        assert!(lookup(&entries, "linux-abc", &[], "v1").is_some());
        assert!(lookup(&entries, "linux-ab", &["linux-ab".into()], "v1").is_some());
        assert!(lookup(&entries, "linux-x", &["linux-ay".into()], "v1").is_none());
    }

    #[test]
    fn カバレッジ検証は隙間重複超過を検出する() {
        println!("チャンクのカバレッジ検証が不正な組み合わせを検出することを確認");
        assert!(validate_coverage(&[(0, 10)], 10));
        assert!(validate_coverage(&[(0, 4), (4, 6)], 10));
        assert!(!validate_coverage(&[(1, 4)], 4)); // 先頭が0でない
        assert!(!validate_coverage(&[(0, 4), (5, 6)], 10)); // 隙間
        assert!(!validate_coverage(&[(0, 4), (3, 6)], 10)); // 重複
        assert!(!validate_coverage(&[(0, 4)], 10)); // 不足
        assert!(validate_coverage(&[(0, 4), (4, 4), (8, 4)], 12));
        assert!(!validate_coverage(&[(0, 4), (4, 0), (4, 8)], 12)); // 0サイズ
        assert!(validate_coverage(&[], 0)); // 空アップロードは 0 バイトとして有効
    }

    #[test]
    fn キーとバージョンの検証ルールがクライアントと一致する() {
        println!("キーとバージョンの検証ルールがクライアント側の制約と一致することを確認");
        assert!(validate_key("a".repeat(512).as_str()));
        assert!(!validate_key("a".repeat(513).as_str()));
        assert!(!validate_key("a,b"));
        assert!(!validate_key(""));
        assert!(validate_version("v"));
        assert!(!validate_version(""));
    }

    #[test]
    fn 予約はttl超過で失効扱いになる() {
        println!("予約がTTLを超えると失効扱いになることを確認");
        let idx = Index {
            next_id: 1,
            entries: vec![],
            reservations: vec![Reservation {
                id: 1,
                key: "k".into(),
                version: "v".into(),
                created_at: 100,
                protocol: "v2".into(),
            }],
        };
        let r = idx.find_reservation(1).unwrap();
        assert!(!idx.is_stale(r, 100 + RESERVATION_TTL_SECS - 1));
        assert!(idx.is_stale(r, 100 + RESERVATION_TTL_SECS));
    }

    #[test]
    fn インデックスのjsonラウンドトリップが壊れない() {
        println!("インデックスのJSON保存と復元が内容を保持することを確認");
        let idx = Index {
            next_id: 3,
            entries: vec![entry(1, "k", "v", 100)],
            reservations: vec![Reservation {
                id: 2,
                key: "k2".into(),
                version: "v".into(),
                created_at: 200,
                protocol: "v1".into(),
            }],
        };
        let bytes = serde_json::to_vec(&idx).unwrap();
        let decoded: Index = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(idx, decoded);
    }
}
