#![recursion_limit = "256"] // TODO: Find out why we reach the default 128 recursion limit

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use clap::{load_yaml, value_t, value_t_or_exit, App, ArgMatches};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use futures::{
    future::FutureExt,
    pin_mut, select,
    stream::{FuturesUnordered, StreamExt},
};
use hdrhistogram::Histogram;
use http::{
    header::{HeaderName, USER_AGENT},
    HeaderMap, Response, StatusCode,
};
use hyper::{
    body::Body,
    client::{connect::HttpConnector, Client, ResponseFuture},
    Method, Request,
};
use hyper_tls::HttpsConnector;
use tokio::{runtime::Builder, time::delay_for};
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
    pub timeout: Duration,
}

#[allow(dead_code)]
struct LoadRunner {
    config: Config,
    http_client: Client<HttpsConnector<HttpConnector>, hyper::Body>,
    ctrlc_rx: Receiver<()>,
    resp_tx: Sender<ResponseInfo>,
    request_count: u64,
    response_count: u64,
    error_count: u64,
    timeout_count: u64,
}

#[derive(Debug, Clone)]
struct ResponseInfo {
    code: StatusCode,
    time: Duration,
    content_lenght: u64,
}

struct WrappedFuture<F> {
    inner: F,
    start: Instant,
}

impl<F> WrappedFuture<F>
where
    F: Future + std::marker::Unpin,
{
    pub fn new(inner: F) -> Self {
        WrappedFuture {
            inner,
            start: Instant::now(),
        }
    }
}

impl<F> Future for WrappedFuture<F>
where
    F: Future + std::marker::Unpin,
{
    type Output = (F::Output, Instant);
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner).poll(cx).map(|v| (v, self.start))
    }
}

impl LoadRunner {
    pub fn new(config: Config, ctrlc_rx: Receiver<()>, resp_tx: Sender<ResponseInfo>) -> LoadRunner {
        let https = HttpsConnector::new();
        let client = Client::builder()
            .pool_max_idle_per_host(config.connections as usize)
            .build::<_, hyper::Body>(https);

        LoadRunner {
            config,
            http_client: client,
            ctrlc_rx,
            resp_tx,
            request_count: 0,
            response_count: 0,
            error_count: 0,
            timeout_count: 0,
        }
    }

    fn build_request(&self) -> Request<Body> {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(self.config.url.as_str())
            .body(Body::default())
            .unwrap();

        let header = req.headers_mut();
        for (k, v) in self.config.headers.iter() {
            header.insert(k, v.clone());
        }
        req
    }

    pub async fn run(&mut self) {
        let start = Instant::now();
        let delay = delay_for(self.config.duration + self.config.timeout).fuse();
        pin_mut!(delay);

        let mut reqs = FuturesUnordered::<WrappedFuture<ResponseFuture>>::new();
        for _ in 0..self.config.connections {
            reqs.push(WrappedFuture::new(self.http_client.request(self.build_request())));
            self.request_count += 1;
        }

        loop {
            crossbeam_channel::select! {
                // Receive Ctrl+C event and close stream.
                recv(self.ctrlc_rx) -> _ => return,
                default => {},
            }

            select! {
                _ = delay => {
                    break;
                },
                (res, req_start) = reqs.select_next_some() => {
                    match res {
                        Err(e) => {
                            self.error_count += 1;
                            println!("Error: {:?}", e);
                        }
                        Ok(mut response) => {
                            self.process_response(response, req_start).await;
                        }
                    }

                    // Start another new request
                    if (start.elapsed() < self.config.duration) {
                        reqs.push(WrappedFuture::new(self.http_client.request(self.build_request())));
                        self.request_count += 1;
                    }
                }
            };
        }
    }

    async fn process_response(&mut self, mut response: Response<Body>, start: Instant) {
        let status = response.status();
        if start.elapsed() >= self.config.timeout {
            self.timeout_count += 1;
            return;
        }

        let timeout = delay_for(self.config.timeout - start.elapsed()).fuse();
        pin_mut!(timeout);
        loop {
            select! {
                _ = timeout => {
                    self.timeout_count += 1;
                    break;
                },
                body_len = response.body_mut().fold(0, |acc, chunk| async move {
                    acc + chunk.unwrap().len()
                }) => {
                    self.response_count += 1;
                    let _ = self.resp_tx.send(ResponseInfo {
                        code: status,
                        time: start.elapsed(),
                        content_lenght: body_len as u64,
                    });
                    break;
                }
            }
        }
    }
}

async fn _main(args: &ArgMatches<'_>) {
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
        Ok(ms) => Duration::new(0, ms * 1_000_000),
        Err(_) => Duration::new(30, 0),
    };

    let mut headers = HeaderMap::new();
    if let Some(custom_headers) = args.values_of("header") {
        for h in custom_headers {
            let kv: Vec<&str> = h.splitn(2, ':').collect();
            if kv.len() == 2 {
                if let Ok(hdr) = HeaderName::from_bytes(kv[0].as_bytes()) {
                    headers.append(hdr, kv[1].trim().parse().unwrap());
                }
            }
        }
    }
    if !headers.contains_key(USER_AGENT) {
        headers.insert(USER_AGENT, "rs-wrk/0.2.0".parse().unwrap());
    }

    let load_config = Config {
        url: url.clone(),
        connections,
        duration,
        headers,
        timeout,
    };
    println!(
        "=> Running {:?} test @ {}\n\t{}ms timeout, {} threads and {} concurrent connections",
        duration,
        url.as_str(),
        timeout.as_millis(),
        threads,
        connections,
    );

    let (tx, rx) = crossbeam_channel::bounded::<ResponseInfo>(1024);
    let mut runner = LoadRunner::new(load_config, ctrlc_events, tx.clone());
    // Drop TX in main thread and when all load threads exit the channel will
    // become disconnected. Then the statisitic thread receives correct Err and
    // exits.
    drop(tx);
    let h = thread::Builder::new()
        .name("summary".to_string())
        .spawn(move || summary(timeout, rx))
        .unwrap();

    runner.run().await;
    drop(runner.resp_tx);
    let _ = h.join();
    println!("\tRequests: {}", runner.request_count);
    println!("\tErrors: {}", runner.error_count);
    println!("\tTimeouts: {}", runner.timeout_count);
}

fn summary(timeout: Duration, rx: Receiver<ResponseInfo>) {
    let mut hist = Histogram::<u64>::new_with_bounds(1, timeout.as_millis() as u64 * 2, 2).unwrap();

    let mut status_codes = HashMap::<StatusCode, u64>::new();
    let mut request_count = 0;
    let mut response_size = 0;
    let t = Instant::now();

    let mut disconnected = false;
    loop {
        loop {
            match rx.try_recv() {
                Ok(info) => {
                    request_count += 1;
                    let latency = info.time;
                    hist.record(latency.as_millis() as u64).unwrap_or(());
                    let counter = status_codes.entry(info.code).or_insert(0);
                    *counter += 1;
                    response_size += info.content_lenght;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let elapsed = t.elapsed();
    println!("Result:");
    println!(
        "\t{} requests in {:?}, {} bytes read",
        request_count, elapsed, response_size,
    );
    println!(
        "\tQPS:        \t{:.2} [#/sec]",
        request_count as f64 / elapsed.as_secs() as f64
    );
    println!(
        "\tThroughput: \t{:.2} [Kbytes/sec]",
        response_size as f64 / 1024.0 / elapsed.as_secs() as f64
    );
    println!();
    println!("\tLatency\tMean\tStdev\tMax\tP99");
    println!(
        "\t\t{:.2}ms\t{:.2}ms\t{:.2}ms\t{:.2}ms",
        hist.mean(),
        hist.stdev(),
        hist.max(),
        hist.value_at_quantile(0.99),
    );

    println!("\tResponse Status: ");
    for (k, v) in status_codes.iter() {
        println!("\t\t{}: {}({:.2}%)", k, v, v / request_count * 100);
    }

    println!("\tPercentage of the requests served within a certain time (ms): ");
    println!("\t\t25%: {:.2}", hist.value_at_quantile(0.25));
    println!("\t\t33%: {:.2}", hist.value_at_quantile(0.33));
    println!("\t\t50%: {:.2}", hist.value_at_quantile(0.5));
    println!("\t\t66%: {:.2}", hist.value_at_quantile(0.66));
    println!("\t\t75%: {:.2}", hist.value_at_quantile(0.75));
    println!("\t\t80%: {:.2}", hist.value_at_quantile(0.8));
    println!("\t\t90%: {:.2}", hist.value_at_quantile(0.9));
    println!("\t\t95%: {:.2}", hist.value_at_quantile(0.95));
    println!("\t\t99%: {:.2}", hist.value_at_quantile(0.99));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_main() {
        let args_def = load_yaml!("rs-wrk.yaml");
        let args_vec = vec![
            "rs-wrk",
            "-d",
            "5",
            "-c",
            "2",
            "-H",
            "User-Agent: rs-wrk/test",
            "-H",
            "X-Custom-Header: FOO",
            "http://localhost/",
        ];
        let args = App::from_yaml(args_def).get_matches_from(args_vec);

        let mut rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(_main(&args)).unwrap();
    }
}

fn main() {
    let args_def = load_yaml!("rs-wrk.yaml");
    let args = App::from_yaml(args_def).get_matches();

    let threads = value_t_or_exit!(args.value_of("threads"), u32);

    let mut rt = Builder::new()
        .threaded_scheduler()
        .enable_all()
        .core_threads(threads as usize)
        .build()
        .unwrap();
    rt.block_on(_main(&args));
}
