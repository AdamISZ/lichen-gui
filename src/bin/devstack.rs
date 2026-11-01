//! A local stack for developing the GUI: a regtest bitcoind, the L2's
//! sequencer, a hub on BABE's mock statement (every claim true, so a
//! dispute needs no proving), a block every few seconds, and a faucet that
//! pays deposits. Each run starts fresh, with a fresh sequencer key: a new
//! deployment, so a GUI wallet pinned to an earlier run's is never shown
//! another history.
//!
//! It writes `<dir>/devstack.json`, the GUI's connection for it:
//! `lichen-gui --devstack <dir>/devstack.json`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bitcoin::Amount;
use bitcoincore_rpc::{Auth, RpcApi};
use clap::Parser;
use lichen_chain::{Chain, Net};
use lichen_hub::{Config, Hub};
use lichen_proto::Statement;
use lichen_sequencer::Sequencer;
use lngap_btc::regtest::Regtest;
use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

#[derive(Parser)]
#[command(about = "A local lichen stack for developing the GUI")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:28300")]
    sequencer: String,
    #[arg(long, default_value = "127.0.0.1:28310")]
    hub: String,
    /// The faucet: `POST /pay {"address", "sats"}`, `POST /mine {"blocks"}`,
    /// `POST /address` (a new address of the node's wallet).
    #[arg(long, default_value = "127.0.0.1:28399")]
    faucet: String,
    /// Seconds between blocks.
    #[arg(long, default_value_t = 3)]
    mine_every: u64,
    /// Wiped at each start.
    #[arg(long, default_value = "devstack")]
    dir: PathBuf,
    /// The hub refuses cooperative withdrawals (the GUI's dispute note).
    #[arg(long)]
    refuse_folds: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let faucet = Server::http(&args.faucet).map_err(|e| anyhow::anyhow!("listen on {}: {e}", args.faucet))?;
    // only a directory a devstack made is wiped
    let marker = args.dir.join(".lichen-devstack");
    if args.dir.exists() && std::fs::read_dir(&args.dir)?.next().is_some() && !marker.exists() {
        bail!("{} has files that aren't a devstack's, and the devstack wipes its directory: name another --dir", args.dir.display());
    }
    let _ = std::fs::remove_dir_all(&args.dir);
    std::fs::create_dir_all(&args.dir)?;
    std::fs::write(&marker, "")?;
    let dir = std::fs::canonicalize(&args.dir)?;

    println!("starting bitcoind (regtest), the sequencer and the hub ...");
    let rt = Arc::new(Regtest::start()?);
    let seq = Sequencer::start(&args.sequencer, Some(dir.join("ledger.json")), lichen_ledger::fresh_key(), lichen_ledger::HUB_KEY)?;
    let hub_chain = Chain::connect(rt.rpc_url(), rt.cookie(), "hub")?;
    // several coins, so the hub can fund more than one contract a block
    for _ in 0..8 {
        rt.fund(&hub_chain.new_spk()?, Amount::from_sat(50_000_000))?;
    }
    let hub = Hub::start(Config {
        network: Net::Regtest,
        listen: args.hub.clone(),
        sequencer: seq.url.clone(),
        rpc_url: rt.rpc_url().to_string(),
        cookie: rt.cookie().clone(),
        wallet: "hub".into(),
        statement: Statement::Babe { mock: true },
        dir: dir.join("hub"),
        seed: "lichen-devstack-hub".into(),
        l2_key: lichen_ledger::regtest_hub(),
        reserve_ppm: 20_000,
        fee_ppm: 10_000,
        max_liquidity: 20_000_000,
        duration: 2_000,
        refuse_folds: args.refuse_folds,
        dispute_true_claims: false,
        first_cid: 1000,
        windows: None,
    })?;
    let conn = json!({
        "network": "regtest",
        "sequencer": seq.url,
        "hub": hub.url,
        "rpc_url": rt.rpc_url(),
        "cookie": rt.cookie(),
        "faucet": format!("http://{}", args.faucet),
    });
    // written whole, and only once everything answers: a GUI waits for it
    let file = dir.join("devstack.json");
    std::fs::write(dir.join("devstack.json.tmp"), serde_json::to_vec_pretty(&conn)?)?;
    std::fs::rename(dir.join("devstack.json.tmp"), &file)?;

    // the sequencer seals, and blocks come, in the background
    let s = seq.url.clone();
    std::thread::spawn(move || loop {
        let _ = lichen_l2::Client::new(&s).and_then(|c| c.seal());
        std::thread::sleep(Duration::from_secs(1));
    });
    let miner = rt.clone();
    let every = Duration::from_secs(args.mine_every);
    std::thread::spawn(move || loop {
        let _ = miner.mine(1);
        std::thread::sleep(every);
    });

    // Ctrl-C: the node stopped and its data removed
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM])?;
    let stop = rt.clone();
    let stale = file.clone();
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            println!("stopping ...");
            // no GUI started later is pointed at a node that's gone
            let _ = std::fs::remove_file(&stale);
            if let Ok(rpc) = bitcoincore_rpc::Client::new(stop.rpc_url(), Auth::CookieFile(stop.cookie().clone())) {
                let _ = rpc.stop();
                let start = Instant::now();
                while rpc.get_block_count().is_ok() && start.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let _ = std::fs::remove_dir_all(stop.datadir());
            std::process::exit(130);
        }
    });

    println!("the devstack: sequencer {}, hub {}, faucet http://{}", seq.url, hub.url, args.faucet);
    println!("the GUI's connection: lichen-gui --devstack {}", file.display());
    for mut req in faucet.incoming_requests() {
        let mut body = String::new();
        let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
        let (code, text) = match route(&rt, req.method(), req.url(), &body) {
            Ok(v) => (200, v.to_string()),
            Err(e) => (400, json!({ "error": format!("{e:#}") }).to_string()),
        };
        let h = Header::from_bytes("Content-Type", "application/json").unwrap();
        let _ = req.respond(Response::from_string(text).with_status_code(code).with_header(h));
    }
    drop(hub);
    Ok(())
}

fn route(rt: &Regtest, method: &Method, url: &str, body: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = if body.trim().is_empty() { json!({}) } else { serde_json::from_str(body)? };
    match (method, url) {
        (Method::Post, "/pay") => {
            let addr: bitcoin::Address<bitcoin::address::NetworkUnchecked> = v["address"].as_str().context("address")?.parse()?;
            let addr = addr.require_network(bitcoin::Network::Regtest)?;
            let sats = v["sats"].as_u64().context("sats")?;
            let (op, _) = rt.fund(&addr.script_pubkey(), Amount::from_sat(sats))?;
            println!("faucet: paid {sats} sats to {addr} ({op})");
            Ok(json!({ "outpoint": op.to_string() }))
        }
        (Method::Post, "/address") => {
            let addr = rt.rpc.get_new_address(None, Some(bitcoincore_rpc::json::AddressType::Bech32m))?.assume_checked();
            Ok(json!({ "address": addr.to_string() }))
        }
        (Method::Post, "/mine") => {
            let n = v["blocks"].as_u64().unwrap_or(1);
            rt.mine(n)?;
            Ok(json!({ "height": rt.height()? }))
        }
        _ => bail!("no {method} {url}"),
    }
}
