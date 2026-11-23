use super::*;
impl QuotaService {
    /// Writes the snapshots next to the accounts, so the last numbers outlive
    /// a restart: a provider refusing quota reads still has something to show.
    pub async fn save(&self) {
        let _guard = self.save_lock.lock().await;
        let path = self.store.quota_snapshot_path();
        let Ok(bytes) = serde_json::to_vec(&*self.cache.read().await) else {
            return;
        };
        if let Err(error) = tokio::task::spawn_blocking(move || write_private(&path, &bytes))
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("save task failed")))
        {
            tracing::warn!(%error, "quota snapshots were not saved");
        }
    }
}
pub(super) fn load_snapshots(path: &std::path::Path) -> BTreeMap<String, QuotaSnapshot> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return BTreeMap::new();
    }
    let mut snapshots: BTreeMap<String, QuotaSnapshot> = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    for snapshot in snapshots.values_mut() {
        snapshot.subscription = snapshot
            .subscription
            .take()
            .map(|tier| tier.with_default_weight(snapshot.provider));
    }
    snapshots
}

/// Replaces `path` atomically with an owner-only file.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("refusing a symlinked snapshot file"));
    }
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}
