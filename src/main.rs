//! `lichen-gui`: a lichen wallet as a page in your browser, served on
//! 127.0.0.1 only. Each launch has a token of its own in the URL it opens,
//! which every call to the API must carry: other pages in the browser
//! can't drive the wallet.

mod app;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use app::{App, AskView, Conn, Launch};
use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("index.html");

#[derive(Parser)]
#[command(about = "A lichen wallet in your browser")]
struct Args {
    /// Where the GUI keeps its wallets and settings.
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:18340")]
    listen: String,
    /// Don't open the browser.
    #[arg(long)]
    no_open: bool,
    /// Connect to a devstack (its `devstack.json`).
    #[arg(long)]
    devstack: Option<PathBuf>,
}

/// The platform's place for an application's data.
fn default_dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME")?);
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/lichen-gui")
    } else {
        std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/share")).join("lichen-gui")
    })
}

fn main() -> Result<()> {
    let args = Args::parse();
    let host = args.listen.split(':').next().unwrap_or_default();
    ensure!(host == "127.0.0.1" || host == "localhost", "the GUI listens on this machine only (127.0.0.1), not {host}");
    let server = Server::http(&args.listen).map_err(|e| anyhow::anyhow!("listen on {}: {e}", args.listen))?;
    let dir = match args.dir {
        Some(d) => d,
        None => default_dir()?,
    };
    let app = App::new(dir.clone())?;
    if let Some(f) = &args.devstack {
        // the devstack writes it once it's up, and removes it when stopped
        if !f.exists() {
            eprintln!("waiting for the devstack to write {} ...", f.display());
            while !f.exists() {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        let conn: Conn = serde_json::from_slice(&std::fs::read(f)?).context("a devstack.json")?;
        app.saved.lock().unwrap().conn = conn;
        app.save()?;
    }
    // reconnect to the last hub, if it answers; the page says so
    let conn = app.conn();
    if !conn.hub.is_empty() {
        let launch = match app.connect(conn.clone()) {
            Ok(_) => Launch { ok: true, text: format!("Connected at launch to the hub at {}: lichen-gui reconnects to the last one when it starts.", conn.hub) },
            Err(e) => {
                eprintln!("not connected: {e:#}");
                Launch { ok: false, text: format!("Not connected at launch to the hub at {}: {e:#}", conn.hub) }
            }
        };
        *app.launch.lock().unwrap() = Some(launch);
    }
    app::refresher(app.clone());
    let mut t = [0u8; 16];
    getrandom::getrandom(&mut t)?;
    let token = hex::encode(t);
    let url = format!("http://{}/?t={token}", args.listen);
    println!("lichen-gui ({}): {url}", dir.display());
    if !args.no_open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).spawn();
    }
    // tiny_http 0.12 ends its listener for good at an error accepting a
    // connection (a reset before accept, too many open files): listen
    // again on the same address, as lichen's sequencer and hub do
    let mut server = server;
    loop {
        match server.recv() {
            Ok(req) => {
                let app = app.clone();
                let token = token.clone();
                std::thread::spawn(move || serve(&app, &token, req));
            }
            Err(e) => {
                eprintln!("lichen-gui: the listener stopped ({e}): listening again on {}", args.listen);
                let mut said = false;
                server = loop {
                    match Server::http(&args.listen) {
                        Ok(s) => break s,
                        Err(e) => {
                            if !said {
                                eprintln!("lichen-gui: listening on {}: {e} (trying each second)", args.listen);
                                said = true;
                            }
                            std::thread::sleep(std::time::Duration::from_secs(1));
                        }
                    }
                };
                eprintln!("lichen-gui: listening again on {}", args.listen);
            }
        }
    }
}

fn serve(app: &Arc<App>, token: &str, mut req: Request) {
    let mut body = String::new();
    let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let (code, text, ct) = if path == "/" {
        (200, PAGE.to_string(), "text/html; charset=utf-8")
    } else {
        let given = req.headers().iter().find(|h| h.field.equiv("X-Lichen-Token")).map(|h| h.value.as_str().to_string());
        let r = if given.as_deref() != Some(token) { Err(anyhow::anyhow!("not this launch's token: open the URL lichen-gui printed")) } else { route(app, req.method(), path, query, &body) };
        match r {
            Ok((v, ct)) => (200, v, ct),
            Err(e) => (400, json!({ "error": format!("{e:#}") }).to_string(), "application/json"),
        }
    };
    let h = Header::from_bytes("Content-Type", ct).unwrap();
    let _ = req.respond(Response::from_string(text).with_status_code(code).with_header(h));
}

fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// `%xx` and `+` decoded (a QR's data).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(v)) => {
                out.push(v);
                i += 2;
            }
            (c, _) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize)]
struct Cid {
    cid: u32,
}

fn route(app: &Arc<App>, method: &Method, path: &str, query: &str, body: &str) -> Result<(String, &'static str)> {
    let j = |v: Value| Ok((v.to_string(), "application/json"));
    let body: Value = if body.trim().is_empty() { json!({}) } else { serde_json::from_str(body).context("a JSON body")? };
    match (method, path) {
        (Method::Get, "/api/state") => {
            let saved = app.saved.lock().unwrap().clone();
            let snap = app.snap.lock().unwrap().clone();
            let job = app.job.lock().unwrap().clone();
            let history = app.history.lock().unwrap().clone();
            let wallet = app.file.lock().unwrap().as_ref().map(|p| p.display().to_string());
            let launch = app.launch.lock().unwrap().clone();
            j(json!({ "saved": saved, "snap": snap, "job": job, "history": history, "pin": app.pending_pin(), "wallet": wallet, "dir": app.dir, "launch": launch }))
        }
        (Method::Post, "/api/connect") => {
            let conn: Conn = serde_json::from_value(body)?;
            *app.launch.lock().unwrap() = None;
            let pin = app.connect(conn)?;
            app.refresh();
            j(json!({ "pin": pin }))
        }
        (Method::Post, "/api/pin") => {
            app.pin()?;
            app.refresh();
            j(json!({ "ok": true }))
        }
        (Method::Get, "/api/quote") => {
            let num = |k: &str| param(query, k).unwrap_or("0").parse::<u64>().with_context(|| format!("{k}: a number of sats"));
            let ask = lichen_wallet::Ask { deposit: num("deposit")?, liquidity: num("liquidity")?, reserve: num("reserve")? };
            let info = match app.snap.lock().unwrap().info.clone() {
                Some(i) => i,
                None => lichen_wallet::hub_info(&app.conn().hub)?,
            };
            let q = ask.quote(&info)?;
            let pay = lichen_wallet::Wallet::deposit_amount(&ask, info.network).to_sat();
            j(json!({ "fee": q.fee, "minted": q.minted, "cap": q.cap, "unit": q.unit, "inbound": q.cap.saturating_sub(q.minted), "pay": pay, "reserve": ask.reserve, "extra": pay - ask.deposit - ask.reserve }))
        }
        (Method::Post, "/api/open") => {
            #[derive(Deserialize)]
            struct Open {
                deposit: u64,
                liquidity: u64,
                reserve: u64,
                payout: String,
            }
            let o: Open = serde_json::from_value(body)?;
            app.open(AskView { deposit: o.deposit, liquidity: o.liquidity, reserve: o.reserve }, o.payout)?;
            j(json!({ "ok": true }))
        }
        (Method::Post, "/api/resume") => {
            #[derive(Deserialize)]
            struct Resume {
                cid: u32,
                ask: AskView,
            }
            let r: Resume = serde_json::from_value(body)?;
            app.resume(r.cid, r.ask)?;
            j(json!({ "ok": true }))
        }
        (Method::Post, "/api/withdraw") => {
            #[derive(Deserialize)]
            struct W {
                cid: u32,
                amount: u64,
            }
            let w: W = serde_json::from_value(body)?;
            let name = if w.amount == 0 { format!("close contract {}, nothing withdrawn", w.cid) } else { format!("withdraw {} sats from contract {}", w.amount, w.cid) };
            app.spawn(&name, move |_, wallet, conn, dir| {
                let info = lichen_wallet::hub_info(&conn.hub)?;
                let statement = app::statement(&info)?;
                match wallet.withdraw(&conn.hub, &statement, dir, w.cid, w.amount) {
                    Ok(txid) if w.amount == 0 => Ok(format!("closed: the fold is {txid}, your reserve paid to the contract's payout address")),
                    Ok(txid) => Ok(format!("withdrew {} sats: the fold is {txid}, paid to the contract's payout address", w.amount)),
                    Err(e) if wallet.returned(w.cid).is_some() => bail!(
                        "{e:#}\n\nYour {} sats are returned to the hub on the L2 and not folded. If the hub was only unreachable, withdraw again (the same return is used). If it refuses, only a dispute on Bitcoin gets them back, before the contract's end. For now, with lichen's CLI, while this GUI is stopped (both use the wallet file):\n\n  {} dispute {} {}\n\nIt tells you what comes next (lichen's docs/USE_A_WALLET.md, \"If the hub won't pay\").",
                        w.amount,
                        cli(conn, &dir.with_extension("json"), &info.statement),
                        w.cid,
                        w.amount
                    ),
                    Err(e) => Err(e),
                }
            })?;
            j(json!({ "ok": true }))
        }
        (Method::Post, "/api/refund") => {
            let c: Cid = serde_json::from_value(body)?;
            app.spawn(&format!("refund the deposit for contract {}", c.cid), move |_, w, conn, _| {
                let txid = w.refund(&conn.chain()?, c.cid, bitcoin::Amount::from_sat(500))?;
                Ok(format!("refunded: {txid}"))
            })?;
            j(json!({ "ok": true }))
        }
        (Method::Post, "/api/sweep") => {
            let c: Cid = serde_json::from_value(body)?;
            app.spawn(&format!("sweep contract {}'s fee budget", c.cid), move |_, w, conn, _| {
                let chain = conn.chain()?;
                // closed by the hub's default, which the wallet didn't post:
                // its output spent, nothing left for the budget to pay
                let funding = w.contracts.iter().find(|h| h.record.cid() == c.cid).map(|h| h.record.funding).context("no such contract")?;
                let spent = !chain.unspent(&funding)?;
                let txid = w.sweep_fees(&chain, c.cid, bitcoin::Amount::from_sat(500), spent)?;
                Ok(format!("swept to the payout address: {txid}"))
            })?;
            j(json!({ "ok": true }))
        }
        (Method::Post, "/api/post-default") => {
            let c: Cid = serde_json::from_value(body)?;
            app.spawn(&format!("post contract {}'s default", c.cid), move |_, w, conn, dir| {
                let info = lichen_wallet::hub_info(&conn.hub)?;
                let txid = w.post_default(&conn.chain()?, &app::statement(&info)?, dir, c.cid)?;
                Ok(format!("posted the default: {txid}, your reserve paid to the contract's payout address"))
            })?;
            j(json!({ "ok": true }))
        }
        // paying from the payout key needs no wallet, so it runs beside an
        // operation (an opening waiting for this very payment)
        (Method::Post, "/api/send-payout") => {
            #[derive(Deserialize)]
            struct S {
                to: String,
                amount: Option<u64>,
            }
            let p: S = serde_json::from_value(body)?;
            let txid = app.send_payout(&p.to, p.amount)?;
            j(json!({ "txid": txid.to_string() }))
        }
        (Method::Post, "/api/pay-deposit") => {
            let pay = app.job.lock().unwrap().pay.clone().context("no deposit to pay: the opening isn't waiting for one")?;
            let txid = app.send_payout(&pay.address, Some(pay.sats))?;
            app.job.lock().unwrap().log.push(format!("paid the deposit from your payout address: {txid}"));
            j(json!({ "txid": txid.to_string() }))
        }
        (Method::Post, "/api/claim-mint") => {
            let c: Cid = serde_json::from_value(body)?;
            app.spawn(&format!("claim contract {}'s mint", c.cid), move |_, w, conn, _| {
                w.claim_mint(&conn.chain()?, c.cid)?;
                Ok("the deposit is yours on the L2".into())
            })?;
            j(json!({ "ok": true }))
        }
        (Method::Post, "/api/send") => {
            #[derive(Deserialize)]
            struct Send {
                to: String,
                amount: u64,
            }
            let s: Send = serde_json::from_value(body)?;
            let to = s.to.trim().to_string();
            ensure!(to.len() != 128, "a shielded address: this GUI pays transparent addresses only, for now");
            let addr: [u8; 32] = hex::decode(&to).ok().and_then(|b| b.try_into().ok()).context("an L2 address is 64 hex characters")?;
            app.spawn(&format!("pay {} sats", s.amount), move |_, w, _, _| {
                let idx = w.send(addr, s.amount, 0)?;
                Ok(format!("paid {} sats to {}…: note {}", s.amount, &to[..16], idx[0]))
            })?;
            j(json!({ "ok": true }))
        }
        (Method::Post, "/api/faucet") => {
            let conn = app.conn();
            ensure!(!conn.faucet.is_empty() && conn.network == lichen_chain::Net::Regtest, "no faucet: this is the devstack's, on regtest");
            let r: Value = ureq_post(&format!("{}/pay", conn.faucet), &body)?;
            j(r)
        }
        (Method::Post, "/api/faucet-address") => {
            let conn = app.conn();
            ensure!(!conn.faucet.is_empty() && conn.network == lichen_chain::Net::Regtest, "no faucet: this is the devstack's, on regtest");
            let r: Value = ureq_post(&format!("{}/address", conn.faucet), &json!({}))?;
            j(r)
        }
        (Method::Get, "/api/qr") => {
            let data = unescape(param(query, "d").unwrap_or_default());
            let code = qrcode::QrCode::new(data.as_bytes())?;
            let svg = code.render::<qrcode::render::svg::Color>().min_dimensions(180, 180).quiet_zone(true).build();
            Ok((svg, "image/svg+xml"))
        }
        _ => bail!("no {method} {path}"),
    }
}

/// lichen's CLI on this GUI's wallet and connection.
fn cli(conn: &Conn, wallet: &std::path::Path, statement: &str) -> String {
    let mut c = format!("lichen --network {} --wallet '{}' --sequencer {} --hub {} --statement {statement}", conn.network, wallet.display(), conn.sequencer, conn.hub);
    if !conn.rpc_url.is_empty() {
        c += &format!(" --rpc-url {} --cookie '{}'", conn.rpc_url, conn.cookie);
    } else if !conn.esplora.is_empty() {
        c += &format!(" --esplora {}", conn.esplora);
    }
    c
}

fn ureq_post(url: &str, body: &Value) -> Result<Value> {
    match ureq::post(url).send_json(body) {
        Ok(r) => Ok(r.into_json()?),
        Err(ureq::Error::Status(_, r)) => bail!("the faucet: {}", r.into_string().unwrap_or_default()),
        Err(e) => Err(e.into()),
    }
}
