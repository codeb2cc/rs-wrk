extern crate clap;
extern crate crossbeam_channel;
extern crate ctrlc;
extern crate futures;
extern crate reqwest;
extern crate tokio;
extern crate url;

use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use clap::{load_yaml, value_t, value_t_or_exit, App, ArgMatches};
use crossbeam_channel::{select, Receiver, Sender, TryRecvError};
use crossbeam_utils::sync::WaitGroup;
use futures::stream::Concat2;
use futures::{Async, Future, Poll, Stream};
use hdrhistogram::Histogram;
use reqwest::header::HeaderMap;
use reqwest::r#async::{Client, Decoder, Response};
use reqwest::Error;
use tokio::prelude::*;
use tokio::timer::Delay;
use url::Url;

fn ctrl_channel() -> Result<Receiver<()>, ctrlc::Error> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    ctrlc::set_handler(move || loop {
        let _ = tx.send(());
    })?;

    Ok(rx)
}

#[derive(Debug, Clone)]
struct Config {
    pub url: url::Url,
    pub connections: u32,
    pub duration: Duration,
    pub headers: HeaderMap,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
struct ResponseInfo {
    code: reqwest::StatusCode,
    time: Duration,
    content_lenght: u64,
}

type OngoingRequest = (
    // Request time
    Instant,
    // Response future
    Box<Future<Item = Response, Error = Error>>,
    // Response Status
    Option<reqwest::StatusCode>,
    // Response body future
    Option<Concat2<Decoder>>,
);

struct LoadRunner {
    url: url::Url,
    duration_delay: Delay,

    http_client: Client,
    ctrlc_rx: Receiver<()>,
    resp_tx: Sender<ResponseInfo>,

    ongoing_requests: Vec<Option<OngoingRequest>>,

    request_count: u64,
    response_count: u64,
    error_count: u64,
}

impl LoadRunner {
    pub fn new(config: Config, ctrlc_rx: Receiver<()>, resp_tx: Sender<ResponseInfo>) -> LoadRunner {
        let concurrency = config.connections as usize;
        let client = Client::builder()
            .timeout(config.timeout.unwrap_or(Duration::from_secs(10)))
            .max_idle_per_host(concurrency)
            .build()
            .unwrap();

        let mut ongoing_requests = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            ongoing_requests.push(None);
        }

        LoadRunner {
            http_client: client,
            url: config.url.clone(),
            duration_delay: Delay::new(Instant::now() + config.duration),
            ctrlc_rx,
            resp_tx,
            ongoing_requests,
            request_count: 0,
            response_count: 0,
            error_count: 0,
        }
    }
}

impl Stream for LoadRunner {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        select! {
            // Receive Ctrl+C event and close stream.
            recv(self.ctrlc_rx) -> _ => return Ok(Async::Ready(None)),
            default => {},
        }

        match self.duration_delay.poll() {
            // Benchmark ends and close stream.
            Ok(Async::Ready(_)) => return Ok(Async::Ready(None)),
            Ok(Async::NotReady) => (),
            Err(_) => return Err(()),
        };

        // Make new request if the old one is finished(None).
        for request in self.ongoing_requests.iter_mut() {
            if request.is_none() {
                *request = Some((
                    Instant::now(),
                    Box::new(self.http_client.get(self.url.as_str()).send()),
                    None,
                    None,
                ));
                self.request_count += 1;
            }
        }

        let mut some_done = false; // Flag to next poll(Async::NotReady)
        for request in self.ongoing_requests.iter_mut() {
            if let Some((start_time, ft, status_code, ft2)) = request {
                if let Some(resp) = ft2 {
                    // Response received, continue to read body
                    match resp.poll() {
                        Ok(Async::Ready(data)) => {
                            let _ = self.resp_tx.send(ResponseInfo {
                                code: status_code.unwrap_or(reqwest::StatusCode::OK),
                                time: Instant::now() - *start_time,
                                content_lenght: data.bytes().count() as u64,
                            });
                            *request = None;
                            some_done = true;
                        }
                        Ok(Async::NotReady) => {}
                        Err(_) => {
                            self.error_count += 1;
                            *request = None;
                        }
                    }
                } else {
                    // Process request
                    match ft.poll() {
                        Ok(Async::Ready(mut resp)) => {
                            self.response_count += 1;
                            let body = mem::replace(resp.body_mut(), Decoder::empty());
                            *status_code = Some(resp.status());
                            *ft2 = Some(body.concat2());
                            some_done = true;
                        }
                        Ok(Async::NotReady) => {}
                        Err(_) => {
                            self.error_count += 1;
                            *request = None;
                        }
                    };
                }
            }
        }

        if some_done {
            Ok(Async::Ready(Some(())))
        } else {
            Ok(Async::NotReady)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_main() {
        let args_def = load_yaml!("rs-wrk.yaml");
        let args_vec = vec!["rs-wrk", "-d", "10", "-t", "1", "-c", "1", "http://localhost/"];
        let args = App::from_yaml(args_def).get_matches_from(args_vec);

        _main(args);
    }
}

fn run(tx: Sender<ResponseInfo>, ctrlc_rx: Receiver<()>, config: Config) {
    let mut rt = tokio::runtime::current_thread::Runtime::new().unwrap();

    let runner = LoadRunner::new(config, ctrlc_rx, tx);
    rt.spawn(runner.for_each(|_| Ok(())));

    let _ = rt.run();
}

fn _main(args: ArgMatches) {
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
    let duration_seconds = value_t_or_exit!(args.value_of("duration"), u32);
    let duration = Duration::new(u64::from(duration_seconds), 0);
    let timeout = match value_t!(args.value_of("timeout"), u32) {
        Ok(seconds) => Some(Duration::new(u64::from(seconds), 0)),
        Err(_) => None,
    };

    let load_config = Config {
        url: url.clone(),
        connections,
        duration,
        headers: HeaderMap::new(),
        timeout,
    };
    println!(
        "=> Running {:?} test @ {}\n\t{} threads and {} connections per thread",
        duration,
        url.as_str(),
        threads,
        connections,
    );

    let hist = match timeout {
        Some(d) => Arc::new(RwLock::new(
            Histogram::<u64>::new_with_bounds(1, d.as_millis() as u64 * 2, 2).unwrap(),
        )),
        None => Arc::new(RwLock::new(Histogram::<u64>::new_with_bounds(1, 60 * 1000, 2).unwrap())),
    };
    let hist_th = hist.clone();

    let status_codes = Arc::new(RwLock::new(HashMap::<reqwest::StatusCode, u64>::new()));
    let request_count = Arc::new(RwLock::new(0));
    let response_size = Arc::new(RwLock::new(0));
    let t = Instant::now();

    let wg = WaitGroup::new();
    let (tx, rx) = crossbeam_channel::bounded::<ResponseInfo>(1024);
    for _ in 0..threads {
        let wg = wg.clone();
        let tx = tx.clone();
        let ctrlc_events = ctrlc_events.clone();
        let config = load_config.clone();

        thread::spawn(move || {
            run(tx, ctrlc_events, config);
            drop(wg);
        });
    }
    // Drop TX in main thread and when all load threads exit the channel will
    // become disconnected. Then the statisitic thread receives correct Err and
    // exits.
    drop(tx);

    let status_codes_th = status_codes.clone();
    let request_count_th = request_count.clone();
    let response_size_th = response_size.clone();
    // Thread to summarize responses statisitic
    thread::spawn(move || {
        let mut hist = hist_th.write().unwrap();
        let mut codes = status_codes_th.write().unwrap();
        let mut count = request_count_th.write().unwrap();
        let mut size = response_size_th.write().unwrap();

        loop {
            select! {
                recv(ctrlc_events) -> _ => {
                    return
                },
                default => loop {
                    match rx.try_recv() {
                        Ok(info) => {
                            *count += 1;
                            let latency = info.time;
                            hist.record(latency.as_millis() as u64).unwrap_or(());
                            let counter = codes.entry(info.code).or_insert(0);
                            *counter += 1;
                            *size += info.content_lenght;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                },
            }
            thread::sleep(Duration::from_millis(10));
        }
    });

    wg.wait();

    let h = hist.read().unwrap();
    let total_requests: u64 = *request_count.read().unwrap();
    let total_bytes: u64 = *response_size.read().unwrap();
    let elapsed = t.elapsed();
    println!("Result:");
    println!(
        "\t{} requests in {:?}, {} bytes read",
        total_requests, elapsed, total_bytes,
    );
    println!(
        "\tQPS:        \t{:.2} [#/sec]",
        total_requests as f64 / elapsed.as_secs() as f64
    );
    println!(
        "\tThroughput: \t{:.2} [Kbytes/sec]",
        total_bytes as f64 / 1024.0 / elapsed.as_secs() as f64
    );
    println!();
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
        println!("\t\t{}: {}({:.2}%)", k, v, v / total_requests * 100);
    }
}

fn main() {
    let args_def = load_yaml!("rs-wrk.yaml");
    let args = App::from_yaml(args_def).get_matches();

    _main(args)
}
