use std::error::Error;
use std::io::{self, Stdout};
use std::time::Duration;

use bdk_kyoto::bip157::tokio;
use bdk_wallet::bitcoin::hex::DisplayHex;
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use kyotod::paths;
use kyotod::server_capnp::server;
use qrcode::{Color as QrColor, QrCode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::net::UnixStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const DEFAULT_DATADIR: &str = "~/.kyotod";

type Backend = CrosstermBackend<Stdout>;
type Term = Terminal<Backend>;

#[derive(Parser)]
#[command(name = "tui", about = "Terminal UI for kyotod")]
struct Cli {
    #[arg(long, default_value = DEFAULT_DATADIR)]
    datadir: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Wallets,
    Wallet,
    Send,
    Result,
    Create,
    Import,
    Network,
    Broadcast,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Unit {
    #[default]
    Sats,
    Btc,
}

impl Unit {
    fn toggle(self) -> Self {
        match self {
            Unit::Sats => Unit::Btc,
            Unit::Btc => Unit::Sats,
        }
    }
    fn format(self, sats: u64) -> String {
        match self {
            Unit::Sats => format!("{sats} sats"),
            Unit::Btc => format!("{:.8} BTC", sats as f64 / 100_000_000.0),
        }
    }
}

#[derive(Default)]
struct App {
    screen_stack: Vec<Screen>,
    unit: Unit,
    wallets: Vec<WalletRow>,
    height: Option<u32>,
    peer_count: Option<usize>,
    progress: Option<f32>,
    network_name: Option<String>,
    list: ListState,
    last_error: Option<String>,
    last_info: Option<String>,
    show_help: bool,
    confirm_shutdown: bool,
    quit: bool,

    // Wallet-detail state (keyed to the currently focused wallet name).
    focus_wallet: Option<String>,
    receive_address: Option<String>,
    history: Option<String>,

    // Forms.
    form: SendForm,
    create: CreateForm,
    import: ImportForm,
    network: NetworkForm,
    broadcast: BroadcastForm,
    required_peers: Option<u8>,
    // Result of the most recent buildTransaction.
    result: Option<BuildResult>,
}

#[derive(Default)]
struct NetworkForm {
    ip: String,
    port: String,
    focus: u8, // 0=ip, 1=port
}

#[derive(Default)]
struct CreateForm {
    name: String,
    external: String,
    change: String,
    birthday: String,
    focus: u8, // 0=name, 1=external, 2=change, 3=birthday
}

#[derive(Default)]
struct ImportForm {
    path: String,
}

#[derive(Default)]
struct BroadcastForm {
    path: String,
    finalize: bool,
    last_txid: Option<String>,
}

struct WalletRow {
    name: String,
    sats: u64,
    active: bool,
}

#[derive(Default)]
struct SendForm {
    recipient: String,
    sats: String,
    sat_per_vb: String,
    out_path: String,
    drain: bool,
    focus: u8, // 0=recipient, 1=sats, 2=sat_per_vb, 3=out_path
}

struct BuildResult {
    psbt_path: String,
    txid: String,
    fee_sats: u64,
    signed: bool,
    raw_tx: Vec<u8>,
    broadcast_txid: Option<String>,
}

impl App {
    fn screen(&self) -> Screen {
        self.screen_stack.last().copied().unwrap_or(Screen::Wallets)
    }
    fn push(&mut self, s: Screen) {
        self.screen_stack.push(s);
    }
    fn pop(&mut self) {
        self.screen_stack.pop();
    }
    fn focused_row(&self) -> Option<&WalletRow> {
        self.list.selected().and_then(|i| self.wallets.get(i))
    }
    fn move_cursor(&mut self, delta: isize) {
        if self.wallets.is_empty() {
            self.list.select(None);
            return;
        }
        let len = self.wallets.len() as isize;
        let cur = self.list.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len) as usize;
        self.list.select(Some(next));
    }
    fn apply(&mut self, snap: Snapshot) {
        if let Some(rows) = snap.wallets {
            self.wallets = rows;
            if self.wallets.is_empty() {
                self.list.select(None);
            } else if self.list.selected().is_none() {
                self.list.select(Some(0));
            } else if self.list.selected().unwrap() >= self.wallets.len() {
                self.list.select(Some(self.wallets.len() - 1));
            }
        }
        if snap.height.is_some() {
            self.height = snap.height;
        }
        if snap.peer_count.is_some() {
            self.peer_count = snap.peer_count;
        }
        if snap.progress.is_some() {
            self.progress = snap.progress;
        }
        if snap.required_peers.is_some() {
            self.required_peers = snap.required_peers;
        }
        if snap.network.is_some() {
            self.network_name = snap.network;
        }
        if let Some(e) = snap.error {
            self.last_error = Some(e);
        }
    }
}

#[derive(Default)]
struct Snapshot {
    wallets: Option<Vec<WalletRow>>,
    height: Option<u32>,
    peer_count: Option<usize>,
    progress: Option<f32>,
    required_peers: Option<u8>,
    network: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
enum Action {
    #[default]
    None,
    Quit,
    OpenWallet,
    Back,
    SetActive,
    RevealAddress,
    OpenSend,
    SubmitSend,
    Broadcast,
    OpenBroadcast,
    SubmitBroadcast,
    OpenCreate,
    OpenImport,
    SubmitCreate,
    SubmitImport,
    ShutdownDaemon,
    OpenNetwork,
    AddPeer,
    IncreasePeers,
    DecreasePeers,
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(tokio::task::LocalSet::new().run_until(async move {
        let client = match connect(&cli.datadir).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
        let mut terminal = setup_terminal()?;
        let res = run_app(&mut terminal, client).await;
        restore_terminal(&mut terminal)?;
        res
    }))
}

async fn connect(datadir: &str) -> Result<server::Client, Box<dyn Error>> {
    let sock = paths::expand(datadir).join("node.sock");
    let stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| format!("connect {}: {e}", sock.display()))?;
    let (r, w) = stream.into_split();
    let r = futures::io::BufReader::new(r.compat());
    let w = futures::io::BufWriter::new(w.compat_write());
    let net = capnp_rpc::twoparty::VatNetwork::new(
        r,
        w,
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc = capnp_rpc::RpcSystem::new(Box::new(net), None);
    let client: server::Client = rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc);
    Ok(client)
}

fn setup_terminal() -> Result<Term, Box<dyn Error>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore_terminal(terminal: &mut Term) -> Result<(), Box<dyn Error>> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

async fn run_app(terminal: &mut Term, client: server::Client) -> Result<(), Box<dyn Error>> {
    let mut app = App::default();
    let mut events = EventStream::new();

    // Poll on a dedicated task so a slow RPC can never starve key input
    // (Ctrl+C, q, navigation). The main loop just races events against fresh
    // snapshots coming over the channel.
    let (snap_tx, mut snap_rx) = tokio::sync::mpsc::channel::<Snapshot>(2);
    let poll_client = client.clone();
    tokio::task::spawn_local(async move {
        // First snapshot fires immediately so the UI isn't blank for 2s.
        if snap_tx.send(poll(&poll_client).await).await.is_err() {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.tick().await; // consume the immediate tick
        loop {
            tick.tick().await;
            let snap = poll(&poll_client).await;
            if snap_tx.send(snap).await.is_err() {
                break;
            }
        }
    });

    loop {
        terminal.draw(|f| draw(f, &app))?;
        if app.quit {
            break;
        }
        let action = tokio::select! {
            biased;
            Some(ev) = events.next() => match ev {
                Ok(event) => handle_event(&mut app, event),
                Err(e) => { app.last_error = Some(format!("input: {e}")); Action::None }
            },
            Some(snap) = snap_rx.recv() => {
                app.apply(snap);
                Action::None
            }
        };
        dispatch(&mut app, action, &client).await;
    }
    Ok(())
}

async fn dispatch(app: &mut App, action: Action, client: &server::Client) {
    match action {
        Action::None => {}
        Action::Quit => app.quit = true,
        Action::OpenWallet => {
            if let Some(row) = app.focused_row() {
                app.focus_wallet = Some(row.name.clone());
                app.receive_address = None;
                app.history = None;
                app.push(Screen::Wallet);
                // Fetch history once on entry.
                if let Err(e) = fetch_history(app, client).await {
                    app.last_error = Some(e);
                }
            }
        }
        Action::Back => {
            app.pop();
            // On full pop-back to the list, clear transient state and stale banners.
            if app.screen() == Screen::Wallets {
                app.focus_wallet = None;
                app.receive_address = None;
                app.history = None;
                app.last_error = None;
                app.last_info = None;
            }
            if app.screen() != Screen::Send {
                app.form = SendForm::default();
            }
        }
        Action::SetActive => {
            let Some(row) = app.focused_row() else { return };
            let name = row.name.clone();
            let mut req = client.set_active_request();
            req.get().set_name(name.as_str());
            match req.send().promise.await {
                Ok(_) => app.last_error = None,
                Err(e) => app.last_error = Some(format!("set-active: {}", clean(&e.to_string()))),
            }
        }
        Action::RevealAddress => {
            if let Err(e) = fetch_receive(app, client).await {
                app.last_error = Some(e);
            }
        }
        Action::OpenSend => {
            app.form = SendForm::default();
            app.push(Screen::Send);
        }
        Action::SubmitSend => {
            match submit_send(&app.form, client).await {
                Ok(r) => {
                    app.result = Some(r);
                    app.push(Screen::Result);
                    app.last_error = None;
                }
                Err(e) => app.last_error = Some(e),
            }
        }
        Action::OpenCreate => {
            app.create = CreateForm::default();
            app.last_error = None;
            app.last_info = None;
            app.push(Screen::Create);
        }
        Action::OpenImport => {
            app.import = ImportForm::default();
            app.last_error = None;
            app.last_info = None;
            app.push(Screen::Import);
        }
        Action::SubmitCreate => match submit_create(&app.create, client).await {
            Ok(msg) => {
                app.create = CreateForm::default();
                app.pop();
                app.last_info = Some(msg);
                app.last_error = None;
            }
            Err(e) => app.last_error = Some(e),
        },
        Action::SubmitImport => match submit_import(&app.import, client).await {
            Ok(msg) => {
                app.import = ImportForm::default();
                app.pop();
                app.last_info = Some(msg);
                app.last_error = None;
            }
            Err(e) => app.last_error = Some(e),
        },
        Action::OpenNetwork => {
            app.network = NetworkForm::default();
            app.last_error = None;
            app.last_info = None;
            app.push(Screen::Network);
        }
        Action::OpenBroadcast => {
            app.broadcast = BroadcastForm::default();
            app.last_error = None;
            app.last_info = None;
            app.push(Screen::Broadcast);
        }
        Action::SubmitBroadcast => {
            let mut req = client.broadcast_psbt_request();
            req.get().set_path(app.broadcast.path.trim());
            req.get().set_finalize(app.broadcast.finalize);
            match req.send().promise.await {
                Ok(resp) => match resp.get().and_then(|r| r.get_txid()) {
                    Ok(t) => {
                        let txid = t.to_string().unwrap_or_default();
                        app.broadcast.last_txid = Some(txid.clone());
                        app.last_info = Some(format!("broadcast {txid}"));
                        app.last_error = None;
                    }
                    Err(e) => app.last_error = Some(format!("broadcast: {e}")),
                },
                Err(e) => {
                    app.last_error = Some(format!("broadcast: {}", clean(&e.to_string())))
                }
            }
        }
        Action::AddPeer => {
            let ip = app.network.ip.trim().to_string();
            if ip.is_empty() {
                app.last_error = Some("ip required".into());
                return;
            }
            let port: u16 = if app.network.port.trim().is_empty() {
                0
            } else {
                match app.network.port.trim().parse() {
                    Ok(p) => p,
                    Err(e) => {
                        app.last_error = Some(format!("port: {e}"));
                        return;
                    }
                }
            };
            let mut req = client.add_peer_request();
            req.get().set_ip(ip.as_str());
            req.get().set_port(port);
            match req.send().promise.await {
                Ok(resp) => match resp.get() {
                    Ok(r) => {
                        let msg = r
                            .get_message()
                            .ok()
                            .and_then(|t| t.to_string().ok())
                            .unwrap_or_default();
                        if r.get_ok() {
                            app.last_info = Some(msg);
                            app.last_error = None;
                            app.network = NetworkForm::default();
                        } else {
                            app.last_error = Some(msg);
                        }
                    }
                    Err(e) => app.last_error = Some(format!("add-peer: {e}")),
                },
                Err(e) => app.last_error = Some(format!("add-peer: {}", clean(&e.to_string()))),
            }
        }
        Action::IncreasePeers => {
            let cur = app.required_peers.unwrap_or(1);
            let next = (cur.saturating_add(1)).min(15);
            send_required_peers(app, client, next).await;
        }
        Action::DecreasePeers => {
            let cur = app.required_peers.unwrap_or(1);
            let next = cur.saturating_sub(1).max(1);
            send_required_peers(app, client, next).await;
        }
        Action::ShutdownDaemon => {
            match client.shutdown_request().send().promise.await {
                Ok(_) => app.quit = true,
                Err(e) => {
                    let msg = clean(&e.to_string());
                    if msg.contains("disconnected") || msg.contains("EOF") || msg.contains("broken pipe") {
                        app.quit = true;
                    } else {
                        app.last_error = Some(format!("shutdown: {msg}"));
                    }
                }
            }
        }
        Action::Broadcast => {
            if let Some(res) = app.result.as_mut() {
                if res.raw_tx.is_empty() {
                    app.last_error = Some("nothing to broadcast (PSBT not signed)".into());
                    return;
                }
                let mut req = client.broadcast_tx_request();
                req.get().set_tx(&res.raw_tx);
                match req.send().promise.await {
                    Ok(resp) => match resp.get().and_then(|r| r.get_txid()) {
                        Ok(t) => {
                            let txid = t.to_string().unwrap_or_default();
                            res.broadcast_txid = Some(txid);
                            app.last_error = None;
                        }
                        Err(e) => app.last_error = Some(format!("broadcast: {e}")),
                    },
                    Err(e) => {
                        app.last_error = Some(format!("broadcast: {}", clean(&e.to_string())))
                    }
                }
            }
        }
    }
}

async fn send_required_peers(app: &mut App, client: &server::Client, n: u8) {
    let mut req = client.set_required_peers_request();
    req.get().set_num(n);
    match req.send().promise.await {
        Ok(resp) => match resp.get() {
            Ok(r) => {
                let msg = r
                    .get_message()
                    .ok()
                    .and_then(|t| t.to_string().ok())
                    .unwrap_or_default();
                if r.get_ok() {
                    app.required_peers = Some(n);
                    app.last_info = Some(msg);
                    app.last_error = None;
                } else {
                    app.last_error = Some(msg);
                }
            }
            Err(e) => app.last_error = Some(format!("set-required-peers: {e}")),
        },
        Err(e) => {
            app.last_error = Some(format!("set-required-peers: {}", clean(&e.to_string())))
        }
    }
}

async fn fetch_history(app: &mut App, client: &server::Client) -> Result<(), String> {
    let resp = client
        .history_request()
        .send()
        .promise
        .await
        .map_err(|e| clean(&e.to_string()))?;
    let text = resp
        .get()
        .and_then(|r| r.get_entries())
        .ok()
        .and_then(|t| t.to_string().ok())
        .unwrap_or_default();
    app.history = Some(text);
    Ok(())
}

async fn fetch_receive(app: &mut App, client: &server::Client) -> Result<(), String> {
    let resp = client
        .receive_request()
        .send()
        .promise
        .await
        .map_err(|e| clean(&e.to_string()))?;
    let addr = resp
        .get()
        .and_then(|r| r.get_address())
        .map_err(|e| e.to_string())?
        .to_string()
        .map_err(|e| e.to_string())?;
    app.receive_address = Some(addr);
    Ok(())
}

async fn submit_create(form: &CreateForm, client: &server::Client) -> Result<String, String> {
    use bdk_wallet::miniscript::{Descriptor, DescriptorPublicKey};
    use bip139::{Account, WalletBackup, BIP_NUMBER, VERSION};

    if form.name.trim().is_empty() {
        return Err("name required".into());
    }
    let ext: Descriptor<DescriptorPublicKey> = form
        .external
        .trim()
        .parse()
        .map_err(|e: bdk_wallet::miniscript::Error| format!("external: {e}"))?;
    let chg: Descriptor<DescriptorPublicKey> = form
        .change
        .trim()
        .parse()
        .map_err(|e: bdk_wallet::miniscript::Error| format!("change: {e}"))?;
    let birthday: Option<u32> = if form.birthday.trim().is_empty() {
        None
    } else {
        Some(
            form.birthday
                .trim()
                .parse()
                .map_err(|e| format!("birthday: {e}"))?,
        )
    };
    let backup = WalletBackup {
        version: Some(VERSION),
        bip: Some(BIP_NUMBER),
        name: Some(form.name.trim().to_string()),
        accounts: vec![Account {
            account_type: Some("bip_380".into()),
            descriptor: Some(ext.into()),
            change_descriptor: Some(chg.into()),
            block_height: birthday,
            ..Default::default()
        }],
        ..Default::default()
    };
    let json = backup
        .to_json()
        .map_err(|e| format!("serialize backup: {e}"))?;
    submit_import_json(client, json).await
}

async fn submit_import(form: &ImportForm, client: &server::Client) -> Result<String, String> {
    let trimmed = form.path.trim();
    if trimmed.is_empty() {
        return Err("path required".into());
    }
    let path = paths::expand(trimmed);
    let json = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    submit_import_json(client, json).await
}

async fn submit_import_json(client: &server::Client, json: String) -> Result<String, String> {
    let mut req = client.import_wallet_request();
    req.get().set_json(json.as_str());
    let resp = req
        .send()
        .promise
        .await
        .map_err(|e| clean(&e.to_string()))?;
    let r = resp.get().map_err(|e| e.to_string())?;
    let name = r
        .get_name()
        .ok()
        .and_then(|t| t.to_string().ok())
        .unwrap_or_default();
    let msg = r
        .get_message()
        .ok()
        .and_then(|t| t.to_string().ok())
        .unwrap_or_default();
    if r.get_ok() {
        Ok(format!("{name}: {msg}"))
    } else {
        Err(format!("{name}: {msg}"))
    }
}

async fn submit_send(form: &SendForm, client: &server::Client) -> Result<BuildResult, String> {
    let sats: u64 = if form.drain {
        0
    } else {
        form.sats
            .trim()
            .parse()
            .map_err(|e| format!("sats: {e}"))?
    };
    let sat_per_vb: f64 = if form.sat_per_vb.trim().is_empty() {
        2.0
    } else {
        form.sat_per_vb
            .trim()
            .parse()
            .map_err(|e| format!("sat/vB: {e}"))?
    };
    let mut req = client.build_transaction_request();
    let mut p = req.get();
    p.set_recipient(form.recipient.trim());
    p.set_sats(sats);
    p.set_sat_per_vb(sat_per_vb);
    p.set_drain(form.drain);
    p.set_out_path(form.out_path.trim());

    let resp = req
        .send()
        .promise
        .await
        .map_err(|e| clean(&e.to_string()))?;
    let r = resp.get().map_err(|e| e.to_string())?;
    Ok(BuildResult {
        psbt_path: r.get_path().ok().and_then(|t| t.to_string().ok()).unwrap_or_default(),
        txid: r.get_txid().ok().and_then(|t| t.to_string().ok()).unwrap_or_default(),
        fee_sats: r.get_fee_sats(),
        signed: r.get_signed(),
        raw_tx: r.get_raw_tx().map(|s| s.to_vec()).unwrap_or_default(),
        broadcast_txid: None,
    })
}

async fn poll(client: &server::Client) -> Snapshot {
    let mut snap = Snapshot::default();
    match client.balances_request().send().promise.await {
        Ok(resp) => match resp.get().and_then(|r| r.get_entries()) {
            Ok(entries) => {
                let rows = entries
                    .iter()
                    .filter_map(|e| {
                        Some(WalletRow {
                            name: e.get_name().ok()?.to_string().ok()?,
                            sats: e.get_sats(),
                            active: e.get_active(),
                        })
                    })
                    .collect();
                snap.wallets = Some(rows);
            }
            Err(e) => snap.error = Some(format!("balances: {e}")),
        },
        Err(e) => snap.error = Some(format!("balances: {}", clean(&e.to_string()))),
    }
    // Bound the Requester-backed calls. During a rebuild the daemon swaps the
    // Requester, and an in-flight chain_tip/peer_info that was dispatched
    // against the previous one can otherwise stall indefinitely. Run them
    // concurrently so the slower one doesn't add to the faster one.
    let to = Duration::from_millis(500);
    let (h, p, g) = tokio::join!(
        tokio::time::timeout(to, client.height_request().send().promise),
        tokio::time::timeout(to, client.peers_request().send().promise),
        tokio::time::timeout(to, client.sync_progress_request().send().promise),
    );
    if let Ok(Ok(resp)) = h {
        if let Ok(r) = resp.get() {
            snap.height = Some(r.get_height());
        }
    }
    if let Ok(Ok(resp)) = p {
        if let Ok(r) = resp.get() {
            if let Ok(entries) = r.get_entries() {
                snap.peer_count = Some(entries.len() as usize);
            }
        }
    }
    if let Ok(Ok(resp)) = g {
        if let Ok(r) = resp.get() {
            if r.get_has_data() {
                snap.progress = Some(r.get_percent());
            }
        }
    }
    if let Ok(resp) = client.get_required_peers_request().send().promise.await {
        if let Ok(r) = resp.get() {
            snap.required_peers = Some(r.get_num());
        }
    }
    if let Ok(resp) = client.network_request().send().promise.await {
        if let Ok(name) = resp.get().and_then(|r| r.get_name()) {
            if let Ok(s) = name.to_string() {
                snap.network = Some(s);
            }
        }
    }
    snap
}

fn clean(msg: &str) -> String {
    let s = msg.strip_prefix("Failed: ").unwrap_or(msg);
    s.strip_prefix("remote exception: ").unwrap_or(s).to_string()
}

// --- input handling -------------------------------------------------------

fn handle_event(app: &mut App, event: Event) -> Action {
    let Event::Key(key) = event else { return Action::None };
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    if key.code == KeyCode::Char('?') {
        app.show_help = !app.show_help;
        return Action::None;
    }
    let on_form = matches!(
        app.screen(),
        Screen::Send | Screen::Create | Screen::Import | Screen::Network | Screen::Broadcast
    );
    if !on_form && key.code == KeyCode::Char('u') {
        app.unit = app.unit.toggle();
        return Action::None;
    }
    if app.show_help {
        if matches!(key.code, KeyCode::Esc) {
            app.show_help = false;
        }
        return Action::None;
    }
    if app.confirm_shutdown {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.confirm_shutdown = false;
                return Action::ShutdownDaemon;
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.confirm_shutdown = false;
            }
            _ => {}
        }
        return Action::None;
    }
    match app.screen() {
        Screen::Wallets => match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                app.move_cursor(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.move_cursor(-1);
                Action::None
            }
            KeyCode::Enter => Action::OpenWallet,
            KeyCode::Char('a') => Action::SetActive,
            KeyCode::Char('c') => Action::OpenCreate,
            KeyCode::Char('i') => Action::OpenImport,
            KeyCode::Char('n') => Action::OpenNetwork,
            KeyCode::Char('b') => Action::OpenBroadcast,
            KeyCode::Char('X') => {
                app.confirm_shutdown = true;
                Action::None
            }
            _ => Action::None,
        },
        Screen::Wallet => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Action::Back,
            KeyCode::Char('r') => Action::RevealAddress,
            KeyCode::Char('s') => Action::OpenSend,
            KeyCode::Char('a') => Action::SetActive,
            _ => Action::None,
        },
        Screen::Send => handle_send(app, key),
        Screen::Create => handle_create(app, key),
        Screen::Import => handle_import(app, key),
        Screen::Network => handle_network(app, key),
        Screen::Broadcast => handle_broadcast(app, key),
        Screen::Result => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Action::Back,
            KeyCode::Char('b') => Action::Broadcast,
            _ => Action::None,
        },
    }
}

fn handle_create(app: &mut App, key: crossterm::event::KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::Back,
        KeyCode::Enter => Action::SubmitCreate,
        KeyCode::Tab => {
            app.create.focus = (app.create.focus + 1) % 4;
            Action::None
        }
        KeyCode::BackTab => {
            app.create.focus = (app.create.focus + 3) % 4;
            Action::None
        }
        KeyCode::Backspace => {
            create_field_mut(&mut app.create).pop();
            Action::None
        }
        KeyCode::Char(c) => {
            create_field_mut(&mut app.create).push(c);
            Action::None
        }
        _ => Action::None,
    }
}

fn create_field_mut(form: &mut CreateForm) -> &mut String {
    match form.focus {
        0 => &mut form.name,
        1 => &mut form.external,
        2 => &mut form.change,
        _ => &mut form.birthday,
    }
}

fn handle_broadcast(app: &mut App, key: crossterm::event::KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::Back,
        KeyCode::Enter => Action::SubmitBroadcast,
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.broadcast.finalize = !app.broadcast.finalize;
            Action::None
        }
        KeyCode::Backspace => {
            app.broadcast.path.pop();
            Action::None
        }
        KeyCode::Char(c) => {
            app.broadcast.path.push(c);
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_network(app: &mut App, key: crossterm::event::KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::Back,
        KeyCode::Enter => Action::AddPeer,
        KeyCode::Tab => {
            app.network.focus = (app.network.focus + 1) % 2;
            Action::None
        }
        KeyCode::BackTab => {
            app.network.focus = (app.network.focus + 1) % 2;
            Action::None
        }
        KeyCode::Char('+') | KeyCode::Char('=') => Action::IncreasePeers,
        KeyCode::Char('-') | KeyCode::Char('_') => Action::DecreasePeers,
        KeyCode::Backspace => {
            network_field_mut(&mut app.network).pop();
            Action::None
        }
        KeyCode::Char(c) => {
            network_field_mut(&mut app.network).push(c);
            Action::None
        }
        _ => Action::None,
    }
}

fn network_field_mut(form: &mut NetworkForm) -> &mut String {
    match form.focus {
        0 => &mut form.ip,
        _ => &mut form.port,
    }
}

fn handle_import(app: &mut App, key: crossterm::event::KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::Back,
        KeyCode::Enter => Action::SubmitImport,
        KeyCode::Backspace => {
            app.import.path.pop();
            Action::None
        }
        KeyCode::Char(c) => {
            app.import.path.push(c);
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_send(app: &mut App, key: crossterm::event::KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::Back,
        KeyCode::Enter => Action::SubmitSend,
        KeyCode::Tab => {
            app.form.focus = (app.form.focus + 1) % 4;
            Action::None
        }
        KeyCode::BackTab => {
            app.form.focus = (app.form.focus + 3) % 4;
            Action::None
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.form.drain = !app.form.drain;
            Action::None
        }
        KeyCode::Backspace => {
            field_mut(&mut app.form).pop();
            Action::None
        }
        KeyCode::Char(c) => {
            field_mut(&mut app.form).push(c);
            Action::None
        }
        _ => Action::None,
    }
}

fn field_mut(form: &mut SendForm) -> &mut String {
    match form.focus {
        0 => &mut form.recipient,
        1 => &mut form.sats,
        2 => &mut form.sat_per_vb,
        _ => &mut form.out_path,
    }
}

// --- drawing --------------------------------------------------------------

fn draw(f: &mut Frame<'_>, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    draw_title(f, chunks[0], app);
    match app.screen() {
        Screen::Wallets => draw_wallets(f, chunks[1], app),
        Screen::Wallet => draw_wallet(f, chunks[1], app),
        Screen::Send => draw_send(f, chunks[1], app),
        Screen::Result => draw_result(f, chunks[1], app),
        Screen::Create => draw_create(f, chunks[1], app),
        Screen::Import => draw_import(f, chunks[1], app),
        Screen::Network => draw_network(f, chunks[1], app),
        Screen::Broadcast => draw_broadcast(f, chunks[1], app),
    }
    draw_status(f, chunks[2], app);
    draw_keys(f, chunks[3], app);
    if app.show_help {
        draw_help(f);
    }
    if app.confirm_shutdown {
        draw_shutdown_confirm(f);
    }
}

fn draw_shutdown_confirm(f: &mut Frame<'_>) {
    let area = f.area();
    let w: u16 = 50.min(area.width.saturating_sub(4));
    let h: u16 = 6.min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" shutdown daemon ")
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(rect);
    let lines = vec![
        Line::from("Stop kyotod? Sync will halt until restart."),
        Line::from(""),
        Line::from(vec![
            Span::styled("y", Style::default().fg(Color::Cyan)),
            Span::raw(" confirm    "),
            Span::styled("n/Esc", Style::default().fg(Color::Cyan)),
            Span::raw(" cancel"),
        ]),
    ];
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_help(f: &mut Frame<'_>) {
    let area = f.area();
    let w: u16 = 60.min(area.width.saturating_sub(4));
    let h: u16 = 20.min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help ")
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(rect);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(Span::styled("global", bold)),
        Line::from(vec![Span::styled("  ? ", dim), Span::raw("help"), Span::styled("    u ", dim), Span::raw("toggle sats/BTC"), Span::styled("    Ctrl+c ", dim), Span::raw("quit")]),
        Line::from(""),
        Line::from(Span::styled("wallets list", bold)),
        Line::from(vec![Span::styled("  j/k ", dim), Span::raw("move    "), Span::styled("Enter ", dim), Span::raw("open    "), Span::styled("c ", dim), Span::raw("create    "), Span::styled("i ", dim), Span::raw("import")]),
        Line::from(vec![Span::styled("  a ", dim), Span::raw("set-active    "), Span::styled("q ", dim), Span::raw("quit    "), Span::styled("X ", dim), Span::raw("shutdown daemon")]),
        Line::from(""),
        Line::from(Span::styled("wallet detail", bold)),
        Line::from(vec![Span::styled("  r ", dim), Span::raw("reveal address    "), Span::styled("s ", dim), Span::raw("send")]),
        Line::from(vec![Span::styled("  a ", dim), Span::raw("set-active        "), Span::styled("Esc ", dim), Span::raw("back")]),
        Line::from(""),
        Line::from(Span::styled("forms (send / create / import)", bold)),
        Line::from(vec![Span::styled("  Tab ", dim), Span::raw("next field    "), Span::styled("Alt+d ", dim), Span::raw("drain (send only)")]),
        Line::from(vec![Span::styled("  Enter ", dim), Span::raw("submit    "), Span::styled("Esc ", dim), Span::raw("back")]),
        Line::from(""),
        Line::from(Span::styled("send result", bold)),
        Line::from(vec![Span::styled("  b ", dim), Span::raw("broadcast (if signed)    "), Span::styled("Esc ", dim), Span::raw("back")]),
    ];
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_title(f: &mut Frame<'_>, area: Rect, app: &App) {
    let label = match app.screen() {
        Screen::Wallets => " kyoto-tui  wallets ",
        Screen::Wallet => " kyoto-tui  wallet ",
        Screen::Send => " kyoto-tui  send ",
        Screen::Result => " kyoto-tui  result ",
        Screen::Create => " kyoto-tui  create wallet ",
        Screen::Import => " kyoto-tui  import wallet ",
        Screen::Network => " kyoto-tui  network ",
        Screen::Broadcast => " kyoto-tui  broadcast ",
    };
    let p = Paragraph::new(Span::styled(
        label,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(p, area);
}

fn draw_wallets(f: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);
    draw_wallet_list(f, rows[0], app);
    draw_sync_gauge(f, rows[1], app);
}

fn draw_sync_gauge(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" sync ");
    match app.progress {
        Some(p) => {
            let pct = p.clamp(0.0, 100.0);
            let gauge = Gauge::default()
                .block(block)
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
                .ratio((pct / 100.0).into())
                .label(format!("{pct:.1}%"));
            f.render_widget(gauge, area);
        }
        None => {
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(
                Paragraph::new("(waiting for first progress event)")
                    .style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
    }
}

fn draw_wallet_list(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" wallets ");
    if app.wallets.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = if app.last_error.is_some() {
            "(unreachable — see status bar)"
        } else {
            "(no wallets — press `c` to add one)"
        };
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = app
        .wallets
        .iter()
        .map(|w| {
            let active = if w.active { " *" } else { "  " };
            let line = Line::from(vec![
                Span::styled(active, Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(
                    pad_right(&w.name, 24),
                    if w.active {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("{:>20}", app.unit.format(w.sats)),
                    Style::default().fg(Color::Yellow),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = app.list.clone();
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_wallet(f: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(name) = app.focus_wallet.as_deref() else {
        return;
    };
    let row = app.wallets.iter().find(|w| w.name == name);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {name} "));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Two columns: left = info + history, right = QR + address.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    // Left
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(cols[0]);
    let balance_line = match row {
        Some(r) => format!(
            "balance: {}\nactive: {}",
            app.unit.format(r.sats),
            if r.active { "yes" } else { "no" },
        ),
        None => "(not in current balances)".to_string(),
    };
    f.render_widget(Paragraph::new(balance_line), left[0]);
    let history = app.history.as_deref().unwrap_or("(loading...)");
    let history_block = Block::default().borders(Borders::TOP).title(" history ");
    f.render_widget(
        Paragraph::new(if history.is_empty() {
            "(no relevant transactions yet)"
        } else {
            history
        })
        .block(history_block)
        .wrap(Wrap { trim: false }),
        left[1],
    );

    // Right
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)])
        .split(cols[1]);
    let qr_lines = match &app.receive_address {
        Some(addr) => qr_paragraph(addr),
        None => vec![Line::from(Span::styled(
            "press 'r' to reveal a new receive address",
            Style::default().fg(Color::DarkGray),
        ))],
    };
    f.render_widget(Paragraph::new(qr_lines), right[0]);
    let addr_text = app.receive_address.as_deref().unwrap_or("");
    f.render_widget(
        Paragraph::new(addr_text).style(Style::default().fg(Color::Cyan)),
        right[1],
    );
}

fn draw_send(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" send ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);
    draw_field(f, rows[0], "recipient", &app.form.recipient, app.form.focus == 0);
    let sats_label = if app.form.drain {
        "sats (ignored — drain mode)"
    } else {
        "sats"
    };
    draw_field(f, rows[1], sats_label, &app.form.sats, app.form.focus == 1);
    draw_field(
        f,
        rows[2],
        "sat/vB (default 2)",
        &app.form.sat_per_vb,
        app.form.focus == 2,
    );
    draw_field(
        f,
        rows[3],
        "psbt out path (default <datadir>/tx.psbt)",
        &app.form.out_path,
        app.form.focus == 3,
    );
    let drain = format!(
        "drain wallet: [{}]   (alt+d to toggle)",
        if app.form.drain { "x" } else { " " }
    );
    f.render_widget(Paragraph::new(drain), rows[4]);
}

fn draw_field(f: &mut Frame<'_>, area: Rect, label: &str, value: &str, focused: bool) {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let line1 = Line::from(Span::styled(label.to_string(), style));
    let line2 = Line::from(vec![
        Span::styled("> ", style),
        Span::raw(value.to_string()),
        if focused {
            Span::styled("▌", Style::default().fg(Color::Cyan))
        } else {
            Span::raw("")
        },
    ]);
    f.render_widget(Paragraph::new(vec![line1, line2]), area);
}

fn draw_result(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" send result ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(res) = app.result.as_ref() else {
        return;
    };
    let mut lines = vec![
        Line::from(format!("psbt:    {}", res.psbt_path)),
        Line::from(format!("txid:    {}", res.txid)),
        Line::from(format!("fee:     {}", app.unit.format(res.fee_sats))),
        Line::from(format!(
            "signed:  {}",
            if res.signed { "yes" } else { "no" }
        )),
    ];
    if let Some(t) = &res.broadcast_txid {
        lines.push(Line::from(format!("broadcast: {t}")));
    } else if res.signed {
        lines.push(Line::from(Span::styled(
            "press 'b' to broadcast",
            Style::default().fg(Color::Cyan),
        )));
        // Show the raw tx hex (truncated) for the curious.
        let hex = res.raw_tx.as_slice().to_lower_hex_string();
        let snippet = if hex.len() > 80 {
            format!("{}…", &hex[..80])
        } else {
            hex
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("raw tx (truncated):", Style::default().fg(Color::DarkGray))));
        lines.push(Line::from(snippet));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_status(f: &mut Frame<'_>, area: Rect, app: &App) {
    let height = app
        .height
        .map(|h| h.to_string())
        .unwrap_or_else(|| "—".into());
    let peers = app
        .peer_count
        .map(|p| p.to_string())
        .unwrap_or_else(|| "—".into());
    let active = app
        .wallets
        .iter()
        .find(|w| w.active)
        .map(|w| w.name.as_str())
        .unwrap_or("—");
    let network = app.network_name.as_deref().unwrap_or("—");
    let mut spans = vec![
        Span::styled(" network ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            network.to_string(),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("height ", Style::default().fg(Color::DarkGray)),
        Span::raw(height),
        Span::raw("   "),
        Span::styled("peers ", Style::default().fg(Color::DarkGray)),
        Span::raw(peers),
        Span::raw("   "),
        Span::styled("active ", Style::default().fg(Color::DarkGray)),
        Span::raw(active.to_string()),
    ];
    if let Some(err) = &app.last_error {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(err.as_str(), Style::default().fg(Color::Red)));
    } else if let Some(info) = &app.last_info {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(info.as_str(), Style::default().fg(Color::Green)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_keys(f: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = match app.screen() {
        Screen::Wallets => vec![
            key(" j/k "), text("move "), key("Enter "), text("open "),
            key("c "), text("create "), key("i "), text("import "),
            key("a "), text("set-active "), key("n "), text("network "),
            key("b "), text("broadcast "),
            key("X "), text("shutdown "), key("q "), text("quit"),
        ],
        Screen::Wallet => vec![
            key(" r "), text("reveal "), key("s "), text("send "),
            key("a "), text("set-active "), key("Esc "), text("back"),
        ],
        Screen::Send => vec![
            key(" Tab "), text("next field "), key("Alt+d "), text("toggle drain "),
            key("Enter "), text("submit "), key("Esc "), text("back"),
        ],
        Screen::Result => vec![
            key(" b "), text("broadcast (if signed) "), key("Esc "), text("back"),
        ],
        Screen::Create => vec![
            key(" Tab "), text("next field "), key("Enter "), text("submit "),
            key("Esc "), text("back"),
        ],
        Screen::Import => vec![
            key(" Enter "), text("submit "), key("Esc "), text("back"),
        ],
        Screen::Network => vec![
            key(" +/- "), text("required peers "), key("Tab "), text("next field "),
            key("Enter "), text("add peer "), key("Esc "), text("back"),
        ],
        Screen::Broadcast => vec![
            key(" Enter "), text("broadcast "), key("Alt+f "), text("toggle finalize "),
            key("Esc "), text("back"),
        ],
    };
    spans.push(text("   "));
    if !matches!(
        app.screen(),
        Screen::Send | Screen::Create | Screen::Import | Screen::Network | Screen::Broadcast
    ) {
        spans.push(key("u "));
        spans.push(text(match app.unit {
            Unit::Sats => "→BTC ",
            Unit::Btc => "→sats ",
        }));
    }
    spans.push(key("? "));
    spans.push(text("help"));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn key(s: &str) -> Span<'_> {
    Span::styled(s.to_string(), Style::default().fg(Color::DarkGray))
}
fn text(s: &str) -> Span<'_> {
    Span::raw(s.to_string())
}

fn pad_right(s: &str, width: usize) -> String {
    if s.chars().count() >= width {
        s.to_string()
    } else {
        let pad = width - s.chars().count();
        let mut out = String::with_capacity(s.len() + pad);
        out.push_str(s);
        for _ in 0..pad {
            out.push(' ');
        }
        out
    }
}

fn draw_create(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" create wallet ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // name
            Constraint::Length(5), // external
            Constraint::Length(5), // change
            Constraint::Length(2), // birthday
            Constraint::Min(0),
        ])
        .split(inner);
    draw_field(f, rows[0], "name", &app.create.name, app.create.focus == 0);
    draw_wrapped_field(
        f,
        rows[1],
        "external descriptor",
        &app.create.external,
        app.create.focus == 1,
    );
    draw_wrapped_field(
        f,
        rows[2],
        "change descriptor",
        &app.create.change,
        app.create.focus == 2,
    );
    draw_field(
        f,
        rows[3],
        "birthday height (optional)",
        &app.create.birthday,
        app.create.focus == 3,
    );
    let hint = Paragraph::new(Line::from(Span::styled(
        "Tab moves between fields. Enter submits. The birthday is the block height the wallet was created at — the node skips filters below it.",
        Style::default().fg(Color::DarkGray),
    )))
    .wrap(Wrap { trim: false });
    f.render_widget(hint, rows[4]);
}

fn draw_network(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" network ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);

    let required = app
        .required_peers
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into());
    let connected = app
        .peer_count
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into());
    let summary = vec![
        Line::from(vec![
            Span::styled("required peers: ", Style::default().fg(Color::DarkGray)),
            Span::styled(required, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled("connected: ", Style::default().fg(Color::DarkGray)),
            Span::raw(connected),
        ]),
        Line::from(Span::styled(
            "press + / - to change required peers (range 1..=15; triggers a light-client rebuild)",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(summary).wrap(Wrap { trim: false }), rows[0]);

    draw_field(f, rows[1], "peer ip", &app.network.ip, app.network.focus == 0);
    draw_field(
        f,
        rows[2],
        "port (blank = default for network)",
        &app.network.port,
        app.network.focus == 1,
    );

    let hint = Paragraph::new(Line::from(Span::styled(
        "Tab moves between fields. Enter adds the peer to the active light client (and to the rebuild whitelist).",
        Style::default().fg(Color::DarkGray),
    )))
    .wrap(Wrap { trim: false });
    f.render_widget(hint, rows[3]);
}

fn draw_import(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" import wallet ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(inner);
    draw_wrapped_field(f, rows[0], "path to BIP-139 JSON", &app.import.path, true);
    let hint = Paragraph::new(Line::from(Span::styled(
        "~/ is expanded. The wallet name is taken from the JSON's `name` field.",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(hint, rows[1]);
}

fn draw_broadcast(f: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" broadcast psbt ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(inner);
    draw_wrapped_field(
        f,
        rows[0],
        "psbt path (default <datadir>/tx.psbt)",
        &app.broadcast.path,
        true,
    );
    let mut lines = vec![
        Line::from(Span::styled(
            "Reads a signed PSBT, extracts the finalized tx, and broadcasts it.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::styled("finalize: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if app.broadcast.finalize { "on" } else { "off" },
                Style::default().fg(if app.broadcast.finalize { Color::Green } else { Color::DarkGray }),
            ),
            Span::styled("   (Alt+f to toggle)", Style::default().fg(Color::DarkGray)),
        ]),
    ];
    if let Some(t) = &app.broadcast.last_txid {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("broadcast: {t}")));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[1]);
}

fn draw_wrapped_field(f: &mut Frame<'_>, area: Rect, label: &str, value: &str, focused: bool) {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let cursor = if focused { "▌" } else { "" };
    let lines = vec![
        Line::from(Span::styled(label.to_string(), style)),
        Line::from(vec![
            Span::styled("> ", style),
            Span::raw(value.to_string()),
            Span::styled(cursor.to_string(), Style::default().fg(Color::Cyan)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

// --- QR -------------------------------------------------------------------

fn qr_paragraph(data: &str) -> Vec<Line<'static>> {
    let code = match QrCode::new(data.as_bytes()) {
        Ok(c) => c,
        Err(_) => {
            return vec![Line::from(Span::styled(
                "(QR encode failed)",
                Style::default().fg(Color::Red),
            ))]
        }
    };
    let w = code.width();
    let quiet = 2;
    let dark = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= w as i32 || y >= w as i32 {
            false
        } else {
            code[(x as usize, y as usize)] == QrColor::Dark
        }
    };
    let total = w + quiet * 2;
    let mut lines = Vec::new();
    let mut y = 0i32;
    let end = total as i32;
    let style = Style::default().fg(Color::White).bg(Color::Black);
    while y < end {
        let mut buf = String::new();
        for x in 0..(total as i32) {
            let top = dark(x - quiet as i32, y - quiet as i32);
            let bot = dark(x - quiet as i32, y + 1 - quiet as i32);
            buf.push(match (top, bot) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(Line::from(Span::styled(buf, style)));
        y += 2;
    }
    lines
}
