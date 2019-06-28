RS-WRK
====

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
    -c, --connections <connections>    total number of HTTP connections to keep open with each thread handling N =
                                       connections/threads [default: 1]
    -d, --duration <duration>          duration of the test in seconds [default: 10]
    -H, --header <header>              HTTP header to add to request, e.g. "User-Agent: wrk"
        --latency <latency>            print detailed latency statistics
    -s, --script <script>              LuaJIT script, see SCRIPTING
    -t, --threads <threads>            total number of threads to use [default: 1]
        --timeout <timeout>            record a timeout if a response is not received within this amount of time.

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