//! The GUI's state: the connection, the wallet (one per network and
//! sequencer key), a snapshot of what the page shows, refreshed in the
//! background, and the one long operation that may be running.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use lichen_chain::{Chain, Net};
use lichen_ledger::Deployment;
use lichen_proto::{Info, Statement};
use lichen_wallet::{Ask, Wallet};
use serde::{Deserialize, Serialize};

/// Where the hub and the chain are.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conn {
    pub network: Net,
    pub sequencer: String,
    pub hub: String,
    /// Esplora servers, comma-separated; empty: the network's public one.
    #[serde(default)]
    pub esplora: String,
    /// Instead of Esplora, a node: its RPC URL and cookie file (a local
    /// regtest's).
    #[serde(default)]
    pub rpc_url: String,
    #[serde(default)]
    pub cookie: String,
    /// The devstack's faucet, which pays deposits on regtest.
    #[serde(default)]
    pub faucet: String,
}

impl Default for Conn {
    fn default() -> Conn {
        Conn { network: Net::Mutinynet, sequencer: String::new(), hub: String::new(), esplora: String::new(), rpc_url: String::new(), cookie: String::new(), faucet: String::new() }
    }
}

impl Conn {
    pub fn chain(&self) -> Result<Chain> {
        if !self.rpc_url.is_empty() {
            ensure!(!self.cookie.is_empty(), "a node's RPC needs its cookie file");
            ensure!(Path::new(&self.cookie).exists(), "the node's cookie file {} doesn't exist (a devstack's, stopped?): clear the node under \"chain source\" to read the chain from Esplora", self.cookie);
            return Chain::connect_on(&self.rpc_url, Path::new(&self.cookie), "lichen-gui", self.network);
        }
        let urls = if self.esplora.trim().is_empty() { self.network.default_esplora().context("no public Esplora for this network: name one, or a node")?.to_string() } else { self.esplora.clone() };
        let urls: Vec<&str> = urls.split(',').map(str::trim).filter(|u| !u.is_empty()).collect();
        Chain::esploras(&urls, self.network)
    }
}

/// What the GUI keeps: the connection, and the last payout address.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub conn: Conn,
    #[serde(default)]
    pub payout: String,
}

/// A deployment's keys, for the user to check against the operator's.
#[derive(Clone, Debug, Serialize)]
pub struct Pin {
    pub sequencer: String,
    pub hub: String,
    pub anchored: bool,
    pub wallet_file: String,
}

impl Pin {
    fn of(d: &Deployment, file: &Path) -> Pin {
        Pin { sequencer: hex::encode(d.sequencer), hub: hex::encode(d.hub), anchored: d.seal.is_some(), wallet_file: file.display().to_string() }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NoteView {
    pub index: u32,
    pub value: u64,
    pub memo: u32,
    pub spent: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ContractView {
    pub cid: u32,
    pub deposit: u64,
    pub minted: u64,
    pub cap: u64,
    pub unit: u64,
    pub t_close: u32,
    pub payout: String,
    /// How it ended: the fold's or the payout's txid.
    pub closed: Option<String>,
    pub mint_claimed: bool,
    /// A return on the L2 not yet folded (sats).
    pub returned: Option<u64>,
    /// A dispute in progress: its `b`, in units.
    pub disputing: Option<u32>,
    /// A closed contract's fee budget left on chain (sats): 0 once swept;
    /// none if open, or not read.
    pub budget: Option<u64>,
    /// Past `T_close`, its output spent with no close of the wallet's: the
    /// hub's default (the user's reserve back).
    pub defaulted: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DepositView {
    pub cid: u32,
    pub address: String,
    pub payout: String,
    /// Blocks after its confirmation before it can be refunded.
    pub timeout: u16,
    /// An offer accepted: the opening may still finish from the chain.
    pub offered: bool,
    /// What was asked when it was opened, if this GUI opened it.
    pub ask: Option<AskView>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct AskView {
    pub deposit: u64,
    pub liquidity: u64,
    pub reserve: u64,
}

impl From<AskView> for Ask {
    fn from(a: AskView) -> Ask {
        Ask { deposit: a.deposit, liquidity: a.liquidity, reserve: a.reserve }
    }
}

/// What the page shows, refreshed in the background.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Snap {
    pub info: Option<Info>,
    pub hub_error: Option<String>,
    pub height: Option<u32>,
    pub chain_error: Option<String>,
    pub wallet_error: Option<String>,
    pub address: String,
    pub transparent: u64,
    pub shielded: u64,
    pub notes: Vec<NoteView>,
    pub contracts: Vec<ContractView>,
    pub deposits: Vec<DepositView>,
    /// The L2's last sealed height, and the last height anchored on
    /// Bitcoin (an anchored deployment's).
    pub sealed: Option<u64>,
    pub anchored: Option<u64>,
    /// The last L2 height the sequencer has proved (read apart from the
    /// wallet, so it moves while an operation runs).
    pub proved: Option<u64>,
    /// The wallet's own Bitcoin payout address, and what's there (sats,
    /// confirmed or not; read apart from the wallet too).
    pub payout_address: String,
    pub payout_balance: Option<u64>,
    /// Unix seconds of the wallet's last refresh.
    pub updated: u64,
    /// Unix seconds of the last refresh's end, the wallet read or not: the
    /// page says when refreshes stop.
    pub refreshed: u64,
}

/// What became of the reconnection to the last hub at launch.
#[derive(Clone, Debug, Serialize)]
pub struct Launch {
    pub ok: bool,
    pub text: String,
}

/// The payment an opening waits for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PayView {
    pub address: String,
    pub sats: u64,
}

/// A long operation: one at a time, the wallet held throughout.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    pub running: bool,
    pub log: Vec<String>,
    pub pay: Option<PayView>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub started: u64,
    pub ended: u64,
}

pub struct App {
    pub dir: PathBuf,
    pub saved: Mutex<Saved>,
    pub wallet: Mutex<Option<Wallet>>,
    /// The wallet's file: its directory (programs, BABE's data) beside it.
    pub file: Mutex<Option<PathBuf>>,
    /// The deployment the connection names, awaiting the user's check
    /// before a wallet is made for it.
    pub pending: Mutex<Option<(Deployment, PathBuf)>>,
    pub snap: Mutex<Snap>,
    pub job: Mutex<Job>,
    pub history: Mutex<Vec<Job>>,
    /// The wallet's closed contracts whose fee budget is spent: it never
    /// fills again, so it isn't read again.
    pub swept: Mutex<std::collections::HashSet<u32>>,
    /// The wallet's payout key, kept apart from it: paying from it needs
    /// no wallet (an opening holds the wallet while it waits for the
    /// deposit this pays).
    pub payout: Mutex<Option<bitcoin::key::Keypair>>,
    /// The wallet's sequencer client (pinned to its deployment), kept apart
    /// from it: a refresh fetches the state before taking the wallet.
    pub seq: Mutex<Option<lichen_l2::Client>>,
    /// The reconnection at launch, until the user connects.
    pub launch: Mutex<Option<Launch>>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl App {
    pub fn new(dir: PathBuf) -> Result<Arc<App>> {
        std::fs::create_dir_all(dir.join("wallets"))?;
        let saved = match std::fs::read(dir.join("config.json")) {
            Ok(b) => serde_json::from_slice(&b).context("config.json")?,
            Err(_) => Saved::default(),
        };
        let history = std::fs::read(dir.join("activity.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Ok(Arc::new(App { dir, saved: Mutex::new(saved), wallet: Mutex::new(None), file: Mutex::new(None), pending: Mutex::new(None), snap: Mutex::new(Snap::default()), job: Mutex::new(Job::default()), history: Mutex::new(history), swept: Default::default(), payout: Mutex::new(None), seq: Mutex::new(None), launch: Mutex::new(None) }))
    }

    pub fn conn(&self) -> Conn {
        self.saved.lock().unwrap().conn.clone()
    }

    pub fn save(&self) -> Result<()> {
        let s = self.saved.lock().unwrap().clone();
        std::fs::write(self.dir.join("config.json"), serde_json::to_vec_pretty(&s)?)?;
        Ok(())
    }

    /// The wallet for a deployment on a network: one per sequencer key,
    /// so that each is pinned to one history.
    fn wallet_path(&self, net: Net, d: &Deployment) -> PathBuf {
        self.dir.join("wallets").join(format!("{net}-{}.json", &hex::encode(d.sequencer)[..16]))
    }

    /// Connect: the hub's offer and the sequencer's deployment, checked
    /// against each other and the network; the wallet for it opened if
    /// there is one, else the deployment held for the user to check
    /// (`pin`).
    pub fn connect(&self, conn: Conn) -> Result<Option<Pin>> {
        ensure!(!self.job.lock().unwrap().running, "an operation is running");
        *self.wallet.lock().unwrap() = None;
        *self.file.lock().unwrap() = None;
        *self.pending.lock().unwrap() = None;
        *self.snap.lock().unwrap() = Snap::default();
        self.swept.lock().unwrap().clear();
        *self.payout.lock().unwrap() = None;
        *self.seq.lock().unwrap() = None;
        {
            let mut s = self.saved.lock().unwrap();
            s.conn = conn.clone();
        }
        self.save()?;
        let info = lichen_wallet::hub_info(&conn.hub).with_context(|| format!("the hub at {}", conn.hub))?;
        ensure!(info.network == conn.network, "the hub is on {}, not {}", info.network, conn.network);
        statement(&info)?;
        conn.chain().and_then(|c| c.height()).context("the chain source")?;
        // the guests: a contract's statement pins them, and this GUI proves
        // with its own build's
        lichen_wallet::check_guests(&info)?;
        let seq = lichen_l2::Client::new(&conn.sequencer).with_context(|| format!("the sequencer at {}", conn.sequencer))?;
        ensure!(seq.deployment == info.deployment, "the hub is in another deployment than the sequencer's: is this the hub's own sequencer?");
        let path = self.wallet_path(conn.network, &info.deployment);
        self.snap.lock().unwrap().info = Some(info.clone());
        if path.exists() {
            let w = Wallet::open(&path, &conn.sequencer)?;
            ensure!(w.sequencer.deployment == info.deployment, "the wallet {} is pinned to another deployment", path.display());
            self.keep_payout(&w, &conn);
            *self.seq.lock().unwrap() = Some(w.sequencer.clone());
            *self.wallet.lock().unwrap() = Some(w);
            *self.file.lock().unwrap() = Some(path);
            return Ok(None);
        }
        let pin = Pin::of(&info.deployment, &path);
        *self.pending.lock().unwrap() = Some((info.deployment, path));
        Ok(Some(pin))
    }

    /// The user has checked the keys: a wallet pinned to the deployment.
    pub fn pin(&self) -> Result<()> {
        let (d, path) = self.pending.lock().unwrap().take().context("nothing to confirm: connect first")?;
        let conn = self.conn();
        let w = Wallet::create(&path, &conn.sequencer)?;
        if w.sequencer.deployment != d {
            let _ = std::fs::remove_file(&path);
            bail!("the sequencer now announces another deployment than the one checked: connect again");
        }
        self.keep_payout(&w, &conn);
        *self.seq.lock().unwrap() = Some(w.sequencer.clone());
        *self.wallet.lock().unwrap() = Some(w);
        *self.file.lock().unwrap() = Some(path);
        Ok(())
    }

    /// The wallet's payout key, kept; on a node, its script watched (a
    /// node's index sees only what's paid after).
    fn keep_payout(&self, w: &Wallet, conn: &Conn) {
        let p = w.payout();
        if let Ok(c) = conn.chain() {
            let _ = c.watch(&p.spk());
        }
        *self.payout.lock().unwrap() = Some(p.keypair);
    }
    /// The payout key, as fees: its coins, and what pays from them.
    pub fn payout_key(&self) -> Option<lichen_chain::KeyFees> {
        self.payout.lock().unwrap().map(|keypair| lichen_chain::KeyFees { keypair, feerate: lichen_wallet::PAYOUT_FEERATE })
    }
    /// Pay from the payout key: `amount`, or everything (less the fee).
    pub fn send_payout(&self, to: &str, amount: Option<u64>) -> Result<bitcoin::Txid> {
        let conn = self.conn();
        let key = self.payout_key().context("no wallet: connect first")?;
        let to: bitcoin::Address<bitcoin::address::NetworkUnchecked> = to.trim().parse().context("the address to pay")?;
        let to = to.require_network(conn.network.bitcoin()).with_context(|| format!("the address to pay isn't a {} address", conn.network))?.script_pubkey();
        lichen_wallet::send_from(&key, &conn.chain()?, &to, amount.map(bitcoin::Amount::from_sat))
    }

    pub fn pending_pin(&self) -> Option<Pin> {
        self.pending.lock().unwrap().as_ref().map(|(d, p)| Pin::of(d, p))
    }

    /// The asks of openings this GUI started, by deposit address (the
    /// wallet keeps the deposit, not what was asked of it).
    fn asks(&self) -> std::collections::BTreeMap<String, AskView> {
        std::fs::read(self.dir.join("asks.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn keep_ask(&self, address: &str, ask: AskView) -> Result<()> {
        let mut all = self.asks();
        all.insert(address.to_string(), ask);
        std::fs::write(self.dir.join("asks.json"), serde_json::to_vec_pretty(&all)?)?;
        Ok(())
    }

    /// Refresh the snapshot: the hub's offer, the chain's height, and,
    /// when no operation holds it, the wallet.
    pub fn refresh(&self) {
        let conn = self.conn();
        if conn.hub.is_empty() {
            return;
        }
        let (info, hub_error) = match lichen_wallet::hub_info(&conn.hub) {
            // a hub upgraded under us: its guests no longer ours
            Ok(i) => {
                let e = lichen_wallet::check_guests(&i).err().map(|e| format!("{e:#}"));
                (Some(i), e)
            }
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let chain = conn.chain();
        let (height, chain_error) = match chain.as_ref().map_err(|e| anyhow::anyhow!("{e:#}")).and_then(|c| c.height()) {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        // apart from the wallet: the proved height, the payout key's coins
        // (an operation may hold the wallet), and the sequencer's state,
        // fetched before the wallet is taken: a slow read then holds up
        // nothing queued for the wallet
        let seq = self.seq.lock().unwrap().clone();
        let proved = seq.as_ref().and_then(|c| c.proved_height().ok().flatten());
        let state = seq.as_ref().map(|c| c.state());
        let payout = self.payout_key().map(|k| {
            let address = bitcoin::Address::from_script(&k.spk(), conn.network.bitcoin()).map_or_else(|_| String::new(), |a| a.to_string());
            (address, chain.as_ref().ok().and_then(|c| k.balance(c).ok()).map(|a| a.to_sat()))
        });
        let wallet = match (self.wallet.try_lock(), state) {
            (Ok(g), Some(state)) => g.as_ref().map(|w| state.and_then(|state| self.read_wallet(w, state, conn.network, chain.as_ref().ok()))),
            _ => None,
        };
        let mut s = self.snap.lock().unwrap();
        if info.is_some() {
            s.info = info;
        }
        s.hub_error = hub_error;
        s.height = height.or(s.height);
        s.chain_error = chain_error;
        match wallet {
            Some(Ok(w)) => {
                let (info, hub_error, height, chain_error) = (s.info.take(), s.hub_error.take(), s.height, s.chain_error.take());
                *s = Snap { info, hub_error, height, chain_error, ..w };
            }
            Some(Err(e)) => s.wallet_error = Some(format!("{e:#}")),
            None => {}
        }
        s.proved = proved;
        if let Some((address, balance)) = payout {
            s.payout_address = address;
            s.payout_balance = balance.or(s.payout_balance);
        }
        s.refreshed = now();
    }

    fn read_wallet(&self, w: &Wallet, state: lichen_l2::State, net: Net, chain: Option<&Chain>) -> Result<Snap> {
        let state = w.observe(state)?;
        let height = chain.and_then(|c| c.height().ok());
        // past T_close with no close of ours: spent means the hub's default
        let defaulted = |h: &lichen_wallet::Held| h.closed.is_none() && height.is_some_and(|t| t >= h.record.terms.t_close) && chain.is_some_and(|c| c.unspent(&h.record.funding).ok() == Some(false));
        let me = w.address();
        let notes = state.transparent().filter(|(_, n)| n.owner == me).map(|(i, n)| NoteView { index: i, value: n.value, memo: n.memo, spent: state.is_spent(n) }).collect();
        let (transparent, shielded) = w.balances_in(&state);
        let addr = |spk: &bitcoin::Script| bitcoin::Address::from_script(spk, net.bitcoin()).map_or_else(|_| spk.to_hex_string(), |a| a.to_string());
        let contracts = w
            .contracts
            .iter()
            .map(|h| {
                let t = &h.record.terms;
                let defaulted = defaulted(h);
                ContractView {
                    cid: h.record.cid(),
                    deposit: u64::from(t.deposit),
                    minted: h.record.minted(),
                    cap: t.v_max().to_sat(),
                    unit: t.unit.to_sat(),
                    t_close: t.t_close,
                    payout: addr(&h.record.pubs[0].payout_spk),
                    closed: h.closed.map(|x| x.to_string()),
                    mint_claimed: h.mint.as_ref().is_none_or(|m| m.claimed),
                    returned: h.closed.is_none().then(|| w.returned(h.record.cid())).flatten().and_then(|i| state.note(i)).map(|n| n.value),
                    disputing: h.claim.as_ref().map(|c| c.0),
                    budget: (h.closed.is_some() || defaulted).then(|| self.budget(w, h, net, chain)).flatten(),
                    defaulted,
                }
            })
            .collect();
        let asks = self.asks();
        let mut deposits = vec![];
        for d in &w.deposits {
            let tree = lichen_proto::deposit_tree(&w.deposit_key(&d.nonce).x_only_public_key().0, &d.hub, d.timeout)?;
            let address = addr(&tree.script_pubkey());
            deposits.push(DepositView { cid: d.cid, ask: asks.get(&address).copied(), address, payout: addr(&d.payout), timeout: d.timeout, offered: d.offered.is_some() });
        }
        let sealed = state.sealed.last().map(|r| r.height);
        let anchored = state.anchors.last().map(|a| a.anchor.height).filter(|&h| h > 0);
        Ok(Snap { address: hex::encode(me), transparent, shielded, notes, contracts, deposits, sealed, anchored, updated: now(), ..Snap::default() })
    }

    /// A closed contract's fee budget left on chain, remembered once spent.
    fn budget(&self, w: &Wallet, h: &lichen_wallet::Held, net: Net, chain: Option<&Chain>) -> Option<u64> {
        let cid = h.record.cid();
        if self.swept.lock().unwrap().contains(&cid) {
            return Some(0);
        }
        let left = w.fees(&h.nonce, net).balance(chain?).ok()?.to_sat();
        if left == 0 {
            self.swept.lock().unwrap().insert(cid);
        }
        Some(left)
    }

    /// Run `f` with the wallet on a thread of its own: one operation at a
    /// time, its progress in the job's log.
    pub fn spawn(self: &Arc<Self>, name: &str, f: impl FnOnce(&App, &mut Wallet, &Conn, &Path) -> Result<String> + Send + 'static) -> Result<()> {
        {
            let mut j = self.job.lock().unwrap();
            ensure!(!j.running, "{} is running", j.name);
            ensure!(self.wallet.lock().unwrap().is_some(), "no wallet: connect first");
            *j = Job { name: name.to_string(), running: true, started: now(), ..Job::default() };
        }
        let app = self.clone();
        std::thread::spawn(move || {
            let conn = app.conn();
            let dir = app.file.lock().unwrap().clone().expect("a wallet's file").with_extension("d");
            let r = {
                let mut g = app.wallet.lock().unwrap();
                let w = g.as_mut().expect("a wallet");
                let log = app.clone();
                w.progress = Some(Arc::new(move |m: String| log.job.lock().unwrap().log.push(m)));
                let r = f(&app, w, &conn, &dir);
                w.progress = None;
                r
            };
            let done = {
                let mut j = app.job.lock().unwrap();
                match r {
                    Ok(m) => j.result = Some(m),
                    Err(e) => j.error = Some(format!("{e:#}")),
                }
                j.running = false;
                j.ended = now();
                j.clone()
            };
            app.remember(done);
            app.refresh();
        });
        Ok(())
    }

    fn remember(&self, job: Job) {
        let mut h = self.history.lock().unwrap();
        h.insert(0, Job { log: vec![], ..job });
        h.truncate(50);
        let _ = std::fs::write(self.dir.join("activity.json"), serde_json::to_vec_pretty(&*h).unwrap_or_default());
    }

    /// Open a contract: the deposit address shown once the hub gives it,
    /// then the opening runs to the mint.
    pub fn open(self: &Arc<Self>, ask: AskView, payout: String) -> Result<()> {
        let net = self.conn().network;
        let spk = payout_spk(&payout, net)?;
        {
            let mut s = self.saved.lock().unwrap();
            s.payout = payout.clone();
        }
        self.save()?;
        self.spawn("open a contract", move |app, w, conn, dir| {
            let info = lichen_wallet::hub_info(&conn.hub)?;
            let statement = statement(&info)?;
            let chain = conn.chain()?;
            let pay = |addr: &bitcoin::Address, amount: bitcoin::Amount| -> Result<()> {
                app.keep_ask(&addr.to_string(), ask)?;
                let mut j = app.job.lock().unwrap();
                j.pay = Some(PayView { address: addr.to_string(), sats: amount.to_sat() });
                j.log.push(format!("pay {} sats to {addr}; waiting for it to confirm", amount.to_sat()));
                Ok(())
            };
            let h = w.open_contract(&conn.hub, &chain, &statement, dir, ask.into(), spk, &pay)?;
            app.job.lock().unwrap().pay = None;
            let t = &h.record.terms;
            Ok(format!("contract {} is open: {} sats minted to you, a cap of {} sats, until block {}", h.record.cid(), h.record.minted(), t.v_max().to_sat(), t.t_close))
        })
    }

    /// Finish an opening whose deposit is paid.
    pub fn resume(self: &Arc<Self>, cid: u32, ask: AskView) -> Result<()> {
        self.spawn(&format!("resume contract {cid}"), move |_, w, conn, dir| {
            let info = lichen_wallet::hub_info(&conn.hub)?;
            let statement = statement(&info)?;
            let chain = conn.chain()?;
            let h = w.resume_open(&conn.hub, &chain, &statement, dir, ask.into(), cid)?;
            Ok(format!("contract {} is open: {} sats minted to you", h.record.cid(), h.record.minted()))
        })
    }
}

/// The hub's statement, if the GUI can run it: BABE's.
pub fn statement(info: &Info) -> Result<Statement> {
    let s: Statement = info.statement.parse()?;
    ensure!(matches!(s, Statement::Babe { .. }), "the hub runs {}; this GUI runs BABE's statements only", info.statement);
    Ok(s)
}

pub fn payout_spk(payout: &str, net: Net) -> Result<bitcoin::ScriptBuf> {
    let a: bitcoin::Address<bitcoin::address::NetworkUnchecked> = payout.trim().parse().context("the payout address")?;
    Ok(a.require_network(net.bitcoin()).with_context(|| format!("the payout address isn't a {net} address"))?.script_pubkey())
}

/// Have glibc's malloc give large blocks back to the system when they're
/// freed. By default it raises its mmap threshold as large blocks are
/// freed (up to 32 MB), and then serves them from each thread's arena,
/// keeping them: after an opening's check of the hub's instances (rebuilt
/// on every core, ~60 MB each at once) the GUI held its peak, about a GB
/// on 16 cores, for as long as it ran, and `malloc_trim` gave back only a
/// quarter of it (an arena's free top stays). A fixed 1 MB threshold: the
/// GUI stays near 40 MB through the check. Called first thing in `main`.
/// macOS returns large blocks by itself; musl has no `mallopt`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn tune_malloc() {
    const M_MMAP_THRESHOLD: std::ffi::c_int = -3;
    extern "C" {
        fn mallopt(param: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
    }
    // SAFETY: glibc's mallopt takes no pointers; called before any thread
    unsafe {
        mallopt(M_MMAP_THRESHOLD, 1 << 20);
    }
}
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn tune_malloc() {}

/// Refresh every few seconds, for the life of the GUI.
pub fn refresher(app: Arc<App>) {
    std::thread::spawn(move || loop {
        app.refresh();
        std::thread::sleep(Duration::from_secs(4));
    });
}
