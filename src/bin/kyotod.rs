use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bdk_kyoto::bip157::tokio;
use bdk_kyoto::{HashCheckpoint, Requester, ScanType};
use bdk_wallet::bitcoin::Network;
use kyotod::daemonize::Daemonize;
use kyotod::ipc::{self, RequesterSlot, ServerArgs};
use kyotod::paths::Layout;
use kyotod::sync::{self, ProgressSlot, SyncHandle};
use kyotod::wallet::State;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};

configure_me::include_config!();

fn main() {
    let (config, _) =
        Config::including_optional_config_files::<&[&str]>(&[]).unwrap_or_exit();

    tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .init();

    let network = Network::from_str(&config.network).expect("invalid network");
    let layout = Arc::new(Layout::new(&config.datadir).expect("failed to prepare data directory"));

    info!(
        target: "node",
        "kyotod starting: network={} datadir={}",
        config.network,
        layout.root.display()
    );

    // Fork BEFORE the tokio runtime exists. fork() only copies the calling
    // thread; if the runtime were already built here, its worker threads,
    // signal driver, and IO driver would not survive into the child and the
    // daemon would silently fail to drive any tasks.
    if config.daemon {
        let working_dir = layout
            .root
            .to_str()
            .expect("datadir is not valid UTF-8")
            .to_owned();
        Daemonize::new(working_dir)
            .fork()
            .expect("failed to daemonize");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run(config, network, layout));
}

async fn run(config: Config, network: Network, layout: Arc<Layout>) {
    let state = Arc::new(Mutex::new(
        State::load(&layout, network).expect("failed to load wallets"),
    ));
    {
        let s = state.lock().unwrap();
        info!(
            target: "wallet",
            "loaded {} wallet(s); active={:?}",
            s.wallets.len(),
            s.active
        );
    }

    let progress: ProgressSlot = Arc::new(Mutex::new(None));
    let mut handle: Option<SyncHandle> = if state.lock().unwrap().wallets.is_empty() {
        info!(target: "node", "no wallets present; waiting for import");
        None
    } else {
        Some(sync::spawn(
            network,
            state.clone(),
            HashMap::new(),
            progress.clone(),
        ))
    };
    let requester_slot: RequesterSlot =
        Arc::new(Mutex::new(handle.as_ref().map(|h| h.requester.clone())));

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (rebuild_tx, mut rebuild_rx) = tokio::sync::mpsc::channel::<()>(1);
    ipc::spawn_server(ServerArgs {
        layout: layout.clone(),
        network,
        shutdown_tx,
        rebuild_tx,
        state: state.clone(),
        requester: requester_slot.clone(),
        progress: progress.clone(),
    });

    let mut sigint = signal(SignalKind::interrupt()).expect("register SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("register SIGTERM handler");
    let cause = loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break "ipc",
            _ = sigint.recv() => break "SIGINT",
            _ = sigterm.recv() => break "SIGTERM",
            Some(_) = rebuild_rx.recv() => {
                info!(target: "node", "rebuilding light client");
                let scans = resolve_scans(&state, handle.as_ref().map(|h| &h.requester)).await;
                *requester_slot.lock().unwrap() = None;
                *progress.lock().unwrap() = None;
                if let Some(h) = handle.take() {
                    sync::shutdown(h).await;
                }
                let h = sync::spawn(network, state.clone(), scans, progress.clone());
                *requester_slot.lock().unwrap() = Some(h.requester.clone());
                handle = Some(h);
                info!(target: "node", "light client rebuilt");
            }
        }
    };
    info!(target: "node", "shutting down ({cause})");

    if let Some(h) = handle {
        sync::shutdown(h).await;
    }
    let sock = layout.socket();
    if let Err(e) = std::fs::remove_file(&sock) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(target: "node", "remove {}: {e}", sock.display());
        }
    }
    if config.daemon {
        let pid = layout.root.join("node.pid");
        let _ = std::fs::remove_file(pid);
    }
    std::process::exit(0);
}

// Look up a HashCheckpoint for each wallet that declared a BIP-139 birthday
// (account.block_height) and hasn't yet synced past it. Hash resolution piggy-
// backs on the *current* (about-to-be-shut-down) light client; if there's no
// such client (first wallet ever) we just return an empty map and the wallet
// falls back to ScanType::Sync from genesis.
async fn resolve_scans(
    state: &Arc<Mutex<State>>,
    requester: Option<&Requester>,
) -> HashMap<String, ScanType> {
    let mut out = HashMap::new();
    let candidates: Vec<(String, u32)> = {
        let s = state.lock().unwrap();
        s.wallets
            .values()
            .filter_map(|w| {
                let birthday = w.backup.accounts.first()?.block_height?;
                let lc = w.wallet.latest_checkpoint().height();
                (lc < birthday).then_some((w.name.clone(), birthday))
            })
            .collect()
    };
    let Some(req) = requester else {
        for (name, h) in candidates {
            warn!(target: "node", "wallet '{name}': birthday {h} but no running client to resolve hash; syncing from genesis");
        }
        return out;
    };
    for (name, h) in candidates {
        match req.get_header(h).await {
            Ok(Some(ih)) => {
                out.insert(
                    name.clone(),
                    ScanType::Recovery {
                        used_script_index: 0,
                        checkpoint: HashCheckpoint::new(h, ih.header.block_hash()),
                    },
                );
                info!(target: "node", "wallet '{name}': starting recovery at height {h}");
            }
            Ok(None) => warn!(target: "node", "wallet '{name}': header at {h} not yet in chain; syncing from genesis"),
            Err(e) => warn!(target: "node", "wallet '{name}': get_header({h}): {e}; syncing from genesis"),
        }
    }
    out
}
