RS-WRK
====

[![Build Status](https://travis-ci.org/codeb2cc/rs-wrk.svg?branch=master)](https://travis-ci.org/codeb2cc/rs-wrk)
[![codecov](https://codecov.io/gh/codeb2cc/rs-wrk/branch/master/graph/badge.svg?token=wIPUy1LeMP)](https://codecov.io/gh/codeb2cc/rs-wrk)

A wrk like HTTP benchmarking tool in Rust.

Usage
----

```
USAGE:
    rs-wrk [OPTIONS] <url>

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
    -c, --connections <connections>    number of concurrent HTTP connections [default: 1]
    -d, --duration <duration>          duration of the test in seconds [default: 10]
    -H, --header <header>...           HTTP header to add to request, e.g. "User-Agent: wrk"
    -t, --threads <threads>            total number of threads to use [default: 1]
        --timeout <timeout>            response timeout in milliseconds

ARGS:
    <url>    benchmark target
```

Report
----

```
=> Running 10s test @ http://localhost/
	2 threads and 20 connections per thread
Result:
	99326 requests in 10.028936138s, 2681802 bytes read
	QPS:        	9932.60 [#/sec]
	Throughput: 	261.89 [Kbytes/sec]

	Latency	Mean	Stdev	Max	P99
		3.46ms	0.62ms	20ms	5ms
	Response Status: 
		200 OK: 99326(100%)
```