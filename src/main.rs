
// copyright 2017 Kaz Wesley

use std::fs::File;
use std::io::BufRead;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cn_stratum::client::{
    ErrorReply, Job, JobAssignment, MessageHandler, PoolClient, PoolClientWriter, RequestId,
};
use yellowsun::{Algo, AllocPolicy, Hasher};

use byteorder::{ByteOrder, LE};
use core_affinity::CoreId;
use log::*;
use serde_derive::{Deserialize, Serialize};

const AGENT: &str = "pow#er/0.2.0";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub address: String,
    pub login: String,
    pub pass: String,
    pub keepalive_s: Option<u64>,
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    pub pool: ClientConfig,
    pub cores: Vec<u32>,
}

fn main() {
    env_logger::init();

    let panicker = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("panicked");
        panicker(info);
        std::process::exit(1);
    }));

    let args = clap::App::new("Pow#er")
        .author("Kaz Wesley <kaz@lambdaverse.org>")
        .arg(
            clap::Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("Sets a custom config file")
                .required(true)
                .takes_value(true),
        ).arg(
            clap::Arg::with_name("allow-slow-mem")
                .long("allow-slow-mem")
                .help("Continue even if hugepages are not available (SLOW!)"),
        ).get_matches();

    let cfg: Config = File::open(args.value_of("config").unwrap())
        .map(serde_json::from_reader)
        .unwrap()
        .unwrap();
    debug!("config: {:?}", &cfg);

    let alloc_policy = if args.is_present("allow-slow-mem") {
        warn!("Slow memory enabled! Performance may be poor.");
        AllocPolicy::AllowSlow
    } else {
        AllocPolicy::RequireFast
    };

    let client = PoolClient::connect(
        &cfg.pool.address,
        &cfg.pool.login,
        &cfg.pool.pass,
        cfg.pool.keepalive_s.map(Duration::from_secs),
        AGENT,
        Client::new,
    ).unwrap();
    let work = client.handler().work();
    let pool = client.write_handle();
    thread::Builder::new()
        .name("poolclient".into())
        .spawn(move || client.run())
        .unwrap();

    let core_ids = core_affinity::get_core_ids().unwrap();
    let worker_count = cfg.cores.len();
    let mut workerstats = Vec::with_capacity(cfg.cores.len());
    for (i, w) in cfg.cores.into_iter().enumerate() {
        let hash_count = Arc::new(AtomicUsize::new(0));
        workerstats.push(Arc::clone(&hash_count));
        let core = core_ids[w as usize];
        debug!("starting worker{} on core {:?}", i, w);
        let worker = Worker {
            hash_count,
            work: Arc::clone(&work),
            pool: Arc::clone(&pool),
            core,
            worker_id: i as u32,
            step: worker_count as u32,
            alloc_policy,
        };
        thread::Builder::new()
            .name(format!("worker{}", i))
            .spawn(move || worker.run())
            .unwrap();
    }

    let mut prevstats: Vec<_> = workerstats
        .iter()
        .map(|w| w.load(Ordering::Relaxed))
        .collect();
    let start = Instant::now();
    let mut prev_start = start;
    let mut total_hashes = 0;
    let stdin = std::io::stdin();
    let mut await_input = stdin.lock().lines();
    loop {
        println!("worker stats (since last):");
        let now = Instant::now();
        let cur_dur = now - prev_start;
        let total_dur = now - start;
        prev_start = now;
        let mut cur_hashes = 0;
        for (i, (prev, new)) in prevstats.iter_mut().zip(&workerstats).enumerate() {
            let new = new.load(Ordering::Relaxed);
            let cur = new - *prev;
            println!("\t{}: {} H/s", i, (cur as f32) / dur_to_f32(&cur_dur));