use std::collections::HashMap;
use std::fs;
use std::path::Path;

use bdk_wallet::bitcoin::Network;
use bdk_wallet::rusqlite::{self, Connection};
use bdk_wallet::{KeychainKind, PersistedWallet, Wallet};
use bip139::WalletBackup;
use tracing::info;

use crate::paths::Layout;

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Json(bip139::Error),
    Invalid(String),
    NetworkMismatch {
        name: String,
        wallet: Network,
        daemon: Network,
    },
    Sqlite(rusqlite::Error),
    Persist(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Json(e) => write!(f, "wallet json: {e}"),
            Self::Invalid(m) => write!(f, "invalid wallet: {m}"),
            Self::NetworkMismatch {
                name,
                wallet,
                daemon,
            } => write!(
                f,
                "{name}: backup is for {wallet:?} but daemon is on {daemon:?}"
            ),
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Persist(m) => write!(f, "wallet persistence: {m}"),
        }
    }
}

impl std::error::Error for LoadError {}

pub struct WalletEntry {
    pub name: String,
    pub backup: WalletBackup,
    pub wallet: PersistedWallet<Connection>,
    pub conn: Connection,
}

pub struct State {
    pub wallets: HashMap<String, WalletEntry>,
    pub active: Option<String>,
}

impl State {
    pub fn load(layout: &Layout, network: Network) -> Result<Self, LoadError> {
        let mut wallets = HashMap::new();
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(layout.wallets_dir()).map_err(LoadError::Io)? {
            let entry = entry.map_err(LoadError::Io)?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| LoadError::Invalid(format!("bad filename {path:?}")))?
                .to_owned();
            let entry = load_one(&name, &path, layout, network)?;
            names.push(name.clone());
            wallets.insert(name, entry);
        }
        names.sort();
        let active = names.into_iter().next();
        Ok(Self { wallets, active })
    }

    pub fn active_entry(&self) -> Option<&WalletEntry> {
        self.active.as_ref().and_then(|n| self.wallets.get(n))
    }

    pub fn active_entry_mut(&mut self) -> Option<&mut WalletEntry> {
        let name = self.active.clone()?;
        self.wallets.get_mut(&name)
    }

    pub fn set_active(&mut self, name: &str) -> Result<(), LoadError> {
        if !self.wallets.contains_key(name) {
            return Err(LoadError::Invalid(format!("no wallet named {name}")));
        }
        self.active = Some(name.to_string());
        Ok(())
    }
}

fn load_one(
    name: &str,
    json_path: &Path,
    layout: &Layout,
    network: Network,
) -> Result<WalletEntry, LoadError> {
    let text = fs::read_to_string(json_path).map_err(LoadError::Io)?;
    let backup = WalletBackup::from_json(&text).map_err(LoadError::Json)?;
    backup.validate().map_err(LoadError::Json)?;
    build_entry(name, backup, layout, network)
}

pub fn build_entry(
    name: &str,
    backup: WalletBackup,
    layout: &Layout,
    network: Network,
) -> Result<WalletEntry, LoadError> {
    if let Some(wnet) = backup.network {
        if wnet != network {
            return Err(LoadError::NetworkMismatch {
                name: name.to_string(),
                wallet: wnet,
                daemon: network,
            });
        }
    }

    let account = backup
        .accounts
        .first()
        .ok_or_else(|| LoadError::Invalid(format!("{name}: no accounts")))?;
    let external = account
        .descriptor
        .as_ref()
        .and_then(|d| d.as_descriptor())
        .cloned()
        .ok_or_else(|| LoadError::Invalid(format!("{name}: missing external descriptor")))?;
    let change = account
        .change_descriptor
        .as_ref()
        .and_then(|d| d.as_descriptor())
        .cloned()
        .ok_or_else(|| LoadError::Invalid(format!("{name}: missing change descriptor")))?;

    let db_path = layout.data_dir().join(format!("{name}.sqlite"));
    let mut conn = Connection::open(&db_path).map_err(LoadError::Sqlite)?;

    let loaded = Wallet::load()
        .descriptor(KeychainKind::External, Some(external.clone()))
        .descriptor(KeychainKind::Internal, Some(change.clone()))
        .check_network(network)
        .load_wallet(&mut conn)
        .map_err(|e| LoadError::Persist(e.to_string()))?;

    let wallet = match loaded {
        Some(w) => {
            info!(target: "wallet", "loaded wallet '{name}' from {}", db_path.display());
            w
        }
        None => {
            let w = Wallet::create(external, change)
                .network(network)
                .create_wallet(&mut conn)
                .map_err(|e| LoadError::Persist(e.to_string()))?;
            info!(target: "wallet", "initialized wallet '{name}' at {}", db_path.display());
            w
        }
    };

    Ok(WalletEntry {
        name: name.to_string(),
        backup,
        wallet,
        conn,
    })
}
