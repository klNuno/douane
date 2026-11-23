use super::*;
impl AccountStore {
    pub(crate) fn oauth_clients_path(&self) -> PathBuf {
        self.directory
            .parent()
            .unwrap_or(&self.directory)
            .join("oauth-clients.json")
    }
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SubscriptionError> {
        let directory = path.into();
        fs::create_dir_all(&directory).map_err(|_| SubscriptionError::Storage)?;
        private_directory(&directory)?;
        let mut accounts = BTreeMap::new();
        for entry in fs::read_dir(&directory).map_err(|_| SubscriptionError::Storage)? {
            let entry = entry.map_err(|_| SubscriptionError::Storage)?;
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            if entry
                .file_type()
                .map_err(|_| SubscriptionError::Storage)?
                .is_symlink()
            {
                return Err(SubscriptionError::Storage);
            }
            let bytes = fs::read(&path).map_err(|_| SubscriptionError::Storage)?;
            let mut account: AccountCredential =
                serde_json::from_slice(&bytes).map_err(|_| SubscriptionError::Storage)?;
            account.subscription = account
                .subscription
                .take()
                .map(|tier| tier.with_default_weight(account.provider));
            if !valid_id(&account.id)
                || path.file_stem().and_then(|v| v.to_str()) != Some(&account.id)
            {
                return Err(SubscriptionError::Storage);
            }
            private_file(&path)?;
            account.migrate_quota();
            account.project_quota();
            accounts.insert(account.id.clone(), account);
        }
        Ok(Self {
            directory,
            accounts: Arc::new(RwLock::new(accounts)),
            refresh_locks: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn reset_state_path(&self) -> PathBuf {
        self.directory.join(".quota-resets")
    }

    pub(crate) fn quota_snapshot_path(&self) -> PathBuf {
        self.directory.join(".quota-snapshots")
    }

    pub async fn delete(&self, id: &str) -> Result<(), SubscriptionError> {
        let mut accounts = self.accounts.write().await;
        if !accounts.contains_key(id) {
            return Err(SubscriptionError::NotFound);
        }
        fs::remove_file(self.directory.join(format!("{id}.json")))
            .map_err(|_| SubscriptionError::Storage)?;
        accounts.remove(id);
        Ok(())
    }

    /// Routes of the custom endpoints currently configured.
    pub(super) fn persist(&self, account: &AccountCredential) -> Result<(), SubscriptionError> {
        Self::persist_in(&self.directory, account)
    }

    pub(super) fn persist_in(
        directory: &Path,
        account: &AccountCredential,
    ) -> Result<(), SubscriptionError> {
        if !valid_id(&account.id) {
            return Err(SubscriptionError::Storage);
        }
        let target = directory.join(format!("{}.json", account.id));
        let temporary = directory.join(format!(".{}.tmp", random_id()));
        let result = (|| {
            let bytes =
                serde_json::to_vec_pretty(account).map_err(|_| SubscriptionError::Storage)?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .map_err(|_| SubscriptionError::Storage)?;
            private_file(&temporary)?;
            file.write_all(&bytes)
                .map_err(|_| SubscriptionError::Storage)?;
            file.sync_all().map_err(|_| SubscriptionError::Storage)?;
            drop(file);
            fs::rename(&temporary, &target).map_err(|_| SubscriptionError::Storage)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}
pub(super) fn valid_id(id: &str) -> bool {
    id.len() == 48 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn private_directory(path: &Path) -> Result<(), SubscriptionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| SubscriptionError::Storage)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let user = std::env::var("USERNAME").map_err(|_| SubscriptionError::Storage)?;
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        let principal = if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        };
        let output = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{principal}:(OI)(CI)F"))
            .creation_flags(0x08000000)
            .output()
            .map_err(|_| SubscriptionError::Storage)?;
        if !output.status.success() {
            return Err(SubscriptionError::Storage);
        }
    }
    Ok(())
}

pub(super) fn private_file(path: &Path) -> Result<(), SubscriptionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| SubscriptionError::Storage)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
