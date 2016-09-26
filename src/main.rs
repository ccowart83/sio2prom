#![allow(unused_must_use)]
#![feature(plugin)]
#![plugin(serde_macros)]

mod sio;

use std::{process, thread};
use std::collections::{HashMap, BTreeMap};
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[macro_use]
extern crate log;
extern crate log4rs;

#[macro_use]
extern crate lazy_static;

extern crate serde;
extern crate serde_json;

extern crate hyper;
use hyper::header::ContentType;
use hyper::mime::Mime;
use hyper::server::{Server, Request, Response};

#[macro_use]
extern crate prometheus;
use prometheus::{Opts, Collector, CounterVec, Gauge, GaugeVec, Histogram, TextEncoder, Encoder};



lazy_static! {
    static ref METRIC_COUNTERS: Mutex<HashMap<String, CounterVec>> = {
        Mutex::new(HashMap::new())
    };
    static ref METRIC_GAUGES: Mutex<HashMap<String, GaugeVec>> = {
        Mutex::new(HashMap::new())
    };

    static ref UPDATE_HISTOGRAM: Histogram = register_histogram!(
        histogram_opts!("sio2prom_update_duration_seconds",
                        "The time in seconds it took to collect the ScaleIO stats"
        )
    ).unwrap();

    static ref HTTP_BODY_GAUGE: Gauge = register_gauge!("sio2prom_http_response_size_bytes",
                                                        "The HTTP response sizes in bytes."
    ).unwrap();

    static ref HTTP_REQ_HISTOGRAM: Histogram = register_histogram!(
        histogram_opts!("sio2prom_http_request_duration_seconds",
                        "The HTTP request latencies in seconds."
        )
    ).unwrap();
}


/// Read json file using `serde_json`
fn read_json(file: &str) -> Option<BTreeMap<String, serde_json::Value>> {
    match File::open(file) {
        Err(e) => panic!("Failed to open file: {}, {:?}", file, e.kind()),
        Ok(mut f) => {
            let mut content: String = String::new();
            f.read_to_string(&mut content).ok().expect("Error reading file");
            let j: serde_json::Value = serde_json::from_str::<serde_json::Value>(&mut content).expect(&format!("Can't deserialize json file {}", file));
            Some(j.as_object().unwrap().clone())
        },
    }
}

fn read_cfg() -> BTreeMap<String, serde_json::Value> {
    let cfg = read_json("cfg/sio2prom.json").unwrap_or_else(|| panic!("Failed to loading config"));
    cfg
}

fn start_exporter(ip: String, port: u64) {
    let encoder = TextEncoder::new();
    let addr: &str = &format!("{}:{}", ip, port).replace('"', "");
    info!("Starting exporter {:?}", addr);

    Server::http(addr)
        .expect("Could not start web server")
        .handle(move |_: Request, mut res: Response| {
            let metric_familys = prometheus::gather();
            let mut buffer = vec![];

            match encoder.encode(&metric_familys, &mut buffer) {
                Ok(_) => {
                    let timer = HTTP_REQ_HISTOGRAM.start_timer();

                    res.headers_mut().set(ContentType(encoder.format_type().parse::<Mime>().unwrap()));
                    match res.send(&buffer) {
                        Err(e) => {
                            error!("Sending responce: {}", e);
                        },
                        _ => (),
                    }

                    timer.observe_duration();
                    HTTP_BODY_GAUGE.set(buffer.len() as f64);
                },
                Err(e) => error!("Encoder problem: {}", e),
            };
        })
        .expect("Could not spawn web server");
}


fn load_prom(metrics: &Vec<sio::metrics::Metric>) {
    let mut counters = METRIC_COUNTERS.lock().expect("Failed to obtain metric counter lock");
    let mut gauges = METRIC_GAUGES.lock().expect("Failed to obtain metric gauge lock");

    for m in metrics {
        // Labels need to be sorted by value https://github.com/pingcap/rust-prometheus/blob/master/src/vec.rs#L78-L80
        let mut labels_sort = m.labels.iter().collect::<Vec<_>>();
        labels_sort.sort_by(|v1, v2| v1.1.cmp(v2.1));
        let labels: Vec<&str> = labels_sort.iter().map(|v| v.0.clone()).collect::<Vec<_>>();

        let opts = Opts::new(m.name.clone(), m.help.clone());

        trace!("Registering metric: {} {:?} ({})", m.name, labels, m.mtype);

        if m.mtype.to_lowercase() == "counter" {
            match register_counter_vec!(opts, &labels) {
                Err(e) => {
                    trace!("Register error: {} {:?} - {}", m.name.clone(), m.labels, e);
                },
                Ok(o) => {
                    counters.insert(m.name.clone().to_string(), o);
                },
            };
        } else if m.mtype.to_lowercase() == "gauge" {
            match register_gauge_vec!(opts, &labels) {
                Err(e) => {
                    trace!("Register error: {} {:?} - {}", m.name.clone(), m.labels, e);
                },
                Ok(o) => {
                    gauges.insert(m.name.clone().to_string(), o);
                },
            };
        } else {
            error!("Unknown metric type: {} {:?} ({})", m.name, labels, m.mtype);
        }

    }
    info!("Loaded metric Counters: {:?}", counters.keys().collect::<Vec<_>>());
    info!("Loaded metric Gauges: {:?}", gauges.keys().collect::<Vec<_>>());
}


fn updata_metrics(metrics: &Vec<sio::metrics::Metric>) {
    let counters = METRIC_COUNTERS.lock().expect("Failed to obtain metric counter lock");
    let gauges = METRIC_GAUGES.lock().expect("Failed to obtain metric gauge lock");

    for m in metrics {
        let mut labels: HashMap<&str, &str> = HashMap::new();
        for (k, v) in m.labels.iter() {
            labels.insert(k, &v);
        }

        if m.mtype.to_lowercase() == "counter" {
            let c = match counters.get(&m.name) {
                None => {
                    error!("The metric {} ({}) was not found as registered", m.name, m.mtype);
                    continue;
                },
                Some(c) => c,
            };

            trace!("Updateing Metric: {:?}", c.collect());

            let metric = match c.get_metric_with(&labels) {
                Err(e) => {
                    error!("The metric {} {:?} ({}) was not found in MetricFamily - {}", m.name, labels, m.mtype, e);
                    continue;
                },
                Ok(m) => m,
            };

            metric.inc_by(m.value as f64);

        } else if m.mtype.to_lowercase() == "gauge" {
            let g = match gauges.get(&m.name) {
                None => {
                    error!("The metric {} ({}) was not found as registered", m.name, m.mtype);
                    continue;
                },
                Some(g) => g,
            };

            trace!("Updateing Metric: {:?}", g.collect());

            let metric = match g.get_metric_with(&labels) {
                Err(e) => {
                    error!("The metric {} {:?} ({}) was not found in MetricFamily - {}", m.name, labels, m.mtype, e);
                    continue;
                },
                Ok(m) => m,
            };

            metric.set(m.value as f64);

        } else {
            error!("Unknown metric type: {} {:?} ({})", m.name, labels, m.mtype);
        }
    }
}


fn scheduler(sio: &Arc<Mutex<sio::client::Client>>, interval: Duration) -> Option<thread::JoinHandle<()>> {
    if interval == Duration::from_secs(0) {
        return None;
    }
    let sio_clone = sio.clone();
    Some(thread::Builder::new()
        .name("scheduler".to_string())
        .spawn(move || {
            loop {
                info!("Starting scheduled metric update");

                match sio::metrics::get_metrics(&sio_clone) {
                    None => error!("Skipping scheduled metric update"),
                    Some(m) => {
                        let timer = UPDATE_HISTOGRAM.start_timer();
                        updata_metrics(&m);
                        timer.observe_duration();
                    },
                }

                thread::sleep(interval);
            }
        })
        .expect("Could not spawn scheduler"))
}


fn main() {
    log4rs::init_file("cfg/log4rs.toml", Default::default()).expect("Failed to initialize logger");

    // TODO Clean this
    let cfg = read_cfg();
    let sio_host = cfg.get("sio").unwrap().as_object().unwrap().get("host").unwrap().to_string().replace('"', "");
    let sio_user = cfg.get("sio").unwrap().as_object().unwrap().get("user").unwrap().to_string().replace('"', "");
    let sio_pass = cfg.get("sio").unwrap().as_object().unwrap().get("pass").unwrap().to_string().replace('"', "");
    let sio_update = cfg.get("sio").unwrap().as_object().unwrap().get("metric_update").unwrap().as_u64().expect("Bad update number");
    let prom_listen_ip = cfg.get("prom").unwrap().as_object().unwrap().get("listen_ip").unwrap().to_string();
    let prom_listen_port: u64 = cfg.get("prom").unwrap().as_object().unwrap().get("listen_port").unwrap().as_u64().expect("Bad port number");

    let sio = sio::client::Client::new(sio_host, sio_user, sio_pass);

    match sio::metrics::get_metrics(&sio) {
        None => {
            process::exit(1);
        },
        Some(m) => load_prom(&m),
    }
    scheduler(&sio, Duration::from_secs(sio_update));

    start_exporter(prom_listen_ip, prom_listen_port);
}
