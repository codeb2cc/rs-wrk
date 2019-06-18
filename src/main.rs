extern crate clap;
extern crate crossbeam_channel;
extern crate ctrlc;
extern crate hdrhistogram;
extern crate reqwest;
extern crate url;

use clap::{load_yaml, value_t, value_t_or_exit, App};
use crossbeam_channel::{bounded, select, Receiver, Sender};
use crossbeam_utils::sync::WaitGroup;
use hdrhistogram::Histogram;
use reqwest::header::HeaderMap;
use reqwest::{Client, Response};
use url::Url;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct RunnerConfig {
    pub url: url::Url,
    pub connections: u32,
    pub duration: Duration,
    pub headers: HeaderMap,
    pub timeout: Option<Duration>,
}

#[derive(Debug)]
struct Load {
    pub start_time: Instant,
    pub end_time: Instant,
    pub response: reqwest::Result<Response>,
}

fn ctrl_channel() -> Result<Receiver<()>, ctrlc::Error> {
    let (tx, rx) = bounded(100);
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })?;

    Ok(rx)
}

fn load_runner(tx: Sender<Load>, ctrlc_rx: Receiver<()>, config: RunnerConfig) {
    let t = Instant::now();
    let client = Client::builder()
        .timeout(config.timeout)
        .max_idle_per_host(config.connections as usize)
        .build()
        .unwrap();

    loop {
        if t.elapsed() > config.duration {
            return;
        }
        select! {
            recv(ctrlc_rx) -> _ => {
                return
            },
            default => {
                let start_time = Instant::now();
                let response = client.get(config.url.as_str()).send();
                let load = Load{
                    start_time,
                    end_time: Instant::now(),
                    response,
                };
                tx.send(load).unwrap();
            },
        }
    }
}

fn main() {
    let args_def = load_yaml!("rs-wrk.yaml");
    let args = App::from_yaml(args_def).get_matches();

    let ctrlc_events = ctrl_channel().unwrap();

    let url = match Url::parse(args.value_of("url").unwrap()) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("URL parse error: {:?}", e);
            std::process::exit(1);
        }
    };

    let threads = value_t_or_exit!(args.value_of("threads"), u32);
    let connections = value_t_or_exit!(args.value_of("connections"), u32);
    let conn_per_thread = (connections as f64 / threads as f64).ceil();
    let duration_seconds = value_t_or_exit!(args.value_of("duration"), u32);
    let duration = Duration::new(duration_seconds as u64, 0);
    let timeout = match value_t!(args.value_of("timeout"), u32) {
        Ok(seconds) => Some(Duration::new(seconds as u64, 0)),
        Err(_) => None,
    };

    let runner_config = RunnerConfig {
        url: url.clone(),
        connections: conn_per_thread as u32,
        duration,
        headers: HeaderMap::new(),
        timeout,
    };
    println!(
        "=> Running {:?} test @ {}\n\t{} threads and {} connections",
        duration,
        url.as_str(),
        threads,
        std::cmp::max(connections, threads),
    );

    let hist = match timeout {
        Some(d) => Arc::new(RwLock::new(
            Histogram::<u64>::new_with_bounds(1, d.as_millis() as u64 * 2, 2).unwrap(),
        )),
        None => Arc::new(RwLock::new(Histogram::<u64>::new_with_bounds(1, 60 * 1000, 2).unwrap())),
    };
    let hist_th = hist.clone();

    let status_codes = Arc::new(RwLock::new(HashMap::<reqwest::StatusCode, u64>::new()));
    let status_codes_th = status_codes.clone();

    let request_count = Arc::new(RwLock::new(0));
    let request_count_th = request_count.clone();
    let response_size = Arc::new(RwLock::new(0));
    let response_size_th = response_size.clone();
    let t = Instant::now();

    let wg = WaitGroup::new();
    let (tx, rx) = crossbeam_channel::unbounded::<Load>();
    for _ in 0..threads {
        let wg = wg.clone();
        let tx = tx.clone();
        let ctrlc_events = ctrlc_events.clone();
        let config = runner_config.clone();

        thread::spawn(move || {
            load_runner(tx, ctrlc_events, config);
            drop(wg);
        });
    }

    // Summarize responses
    thread::spawn(move || loop {
        let mut hist = hist_th.write().unwrap();
        let mut status_codes = status_codes_th.write().unwrap();
        let mut count = request_count_th.write().unwrap();
        let mut size = response_size_th.write().unwrap();

        select! {
            recv(ctrlc_events) -> _ => {
                return
            },
            default => {
                while let Some(load) = rx.try_iter().next() {
                    *count += 1;
                    let latency = load.end_time - load.start_time;
                    hist.record(latency.as_millis() as u64).unwrap();

                    match load.response {
                        Ok(mut response) => {
                            let counter = status_codes.entry(response.status()).or_insert(0);
                            *counter += 1;

                            let mut buf: Vec<u8> = vec![];
                            let len = response.copy_to(&mut buf).unwrap_or(0);
                            *size += len;
                        }
                        Err(e) => println!("{:?}", e),
                    }
                }
            },
        }
        thread::sleep(Duration::from_millis(10));
    });

    wg.wait();

    let h = hist.read().unwrap();
    println!("Result:");
    println!(
        "\t{} requests in {:?}, {} bytes read",
        request_count.read().unwrap(),
        t.elapsed(),
        response_size.read().unwrap()
    );
    println!("\tLatency\tMean\tStdev\tMax\tP99");
    println!(
        "\t\t{:.2}ms\t{:.2}ms\t{:.2}ms\t{:.2}ms",
        h.mean(),
        h.stdev(),
        h.max(),
        h.value_at_quantile(0.99),
    );

    println!("\tResponse Status: ");
    for (k, v) in status_codes.read().unwrap().iter() {
        println!("\t\t{}: {}", k, v);
    }
}