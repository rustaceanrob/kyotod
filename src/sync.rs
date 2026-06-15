use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bdk_kyoto::builder::{Builder, BuilderExt};
use bdk_kyoto::{
    bip157::tokio, wallets, Info, Receiver, Requester, ScanType, UnboundedReceiver, Update,
    UpdateSubscriber, Warning,
};

pub type ProgressSlot = Arc<Mutex<Option<f32>>>;
use bdk_wallet::bitcoin::Network;
use bdk_wallet::chain::{DescriptorExt, DescriptorId};
use bdk_wallet::KeychainKind;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::wallet::State;

pub struct SyncHandle {
    pub requester: Requester,
    log_task: JoinHandle<()>,
    update_task: JoinHandle<()>,
}

pub fn spawn(
    network: Network,
    state: Arc<Mutex<State>>,
    scan_overrides: HashMap<String, ScanType>,
    progress: ProgressSlot,
) -> SyncHandle {
    let client = {
        let guard = state.lock().unwrap();
        let wallets: Vec<_> = guard
            .wallets
            .values()
            .map(|w| {
                let scan = scan_overrides
                    .get(&w.name)
                    .copied()
                    .unwrap_or(ScanType::Sync);
                if !matches!(scan, ScanType::Sync) {
                    info!(target: "node", "wallet '{}' starting in recovery mode", w.name);
                }
                (&*w.wallet, scan)
            })
            .collect();
        Builder::new(network)
            .build_with_wallets(wallets)
            .expect("failed to build light client")
    };

    let (client, logging, update_subscriber) = client.subscribe();
    let client = client.start();
    let requester = client.requester();

    let log_task = tokio::spawn(forward_logs(
        progress,
        logging.info_subscriber,
        logging.warning_subscriber,
    ));
    let update_task = tokio::spawn(apply_updates(update_subscriber, state));

    SyncHandle {
        requester,
        log_task,
        update_task,
    }
}

pub async fn shutdown(handle: SyncHandle) {
    if let Err(e) = handle.requester.shutdown() {
        warn!(target: "node", "requester.shutdown: {e}");
    }
    handle.update_task.abort();
    handle.log_task.abort();
}

async fn forward_logs(
    progress: ProgressSlot,
    mut info_rx: Receiver<Info>,
    mut warn_rx: UnboundedReceiver<Warning>,
) {
    info!(target: "node", "log forwarder started");
    let mut info_open = true;
    let mut warn_open = true;
    while info_open || warn_open {
        tokio::select! {
            i = info_rx.recv(), if info_open => match i {
                Some(msg) => {
                    if let Info::Progress(p) = &msg {
                        *progress.lock().unwrap() = Some(p.percentage_complete());
                    }
                    info!(target: "node", "{msg}");
                }
                None => {
                    info!(target: "node", "info channel closed");
                    info_open = false;
                }
            },
            w = warn_rx.recv(), if warn_open => match w {
                Some(msg) => warn!(target: "node", "{msg}"),
                None => {
                    info!(target: "node", "warning channel closed");
                    warn_open = false;
                }
            },
        }
    }
    info!(target: "node", "log forwarder exiting");
}

async fn apply_updates(
    mut subscriber: UpdateSubscriber<wallets::Multiple>,
    state: Arc<Mutex<State>>,
) {
    info!(target: "node", "update task started; waiting for sync to tip");
    loop {
        let updates = match subscriber.updates().await {
            Ok(u) => u,
            Err(e) => {
                error!(target: "node", "update subscriber: {e}");
                break;
            }
        };
        let mut state = state.lock().unwrap();
        for (desc_id, update) in updates {
            apply_one(&mut state, desc_id, update);
        }
    }
    info!(target: "node", "update task exiting");
}

fn apply_one(state: &mut State, desc_id: DescriptorId, update: Update) {
    let Some(entry) = state.wallets.values_mut().find(|e| {
        e.wallet
            .public_descriptor(KeychainKind::External)
            .descriptor_id()
            == desc_id
    }) else {
        warn!(target: "wallet", "received update for unknown descriptor {desc_id:?}");
        return;
    };
    if let Err(e) = entry.wallet.apply_update(update) {
        error!(target: "wallet", "wallet '{}' apply: {e}", entry.name);
        return;
    }
    match entry.wallet.persist(&mut entry.conn) {
        Ok(_) => info!(
            target: "wallet",
            "wallet '{}' synced to height {}",
            entry.name,
            entry.wallet.local_chain().tip().height(),
        ),
        Err(e) => error!(target: "wallet", "wallet '{}' persist: {e}", entry.name),
    }
}
