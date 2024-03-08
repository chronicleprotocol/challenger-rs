use lazy_static::lazy_static;
use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry};

lazy_static! {
    pub static ref REGISTRY: Registry =
        Registry::new_custom(Some(String::from("challenger")), None)
            .expect("registry can be created");
    pub static ref ERRORS_COUNTER: IntCounterVec = IntCounterVec::new(
        Opts::new("errors_total", "Challenger Errors Counter"),
        &["address", "from", "error"]
    )
    .expect("metric can be created");
    pub static ref CHALLENGE_COUNTER: IntCounterVec = IntCounterVec::new(
        Opts::new("challenges_total", "Number of challenges made"),
        &["address", "from", "tx"]
    )
    .expect("metric can be created");
    pub static ref LAST_SCANNED_BLOCK_GAUGE: IntGaugeVec = IntGaugeVec::new(
        Opts::new("last_scanned_block", "Last scanned block"),
        &["address", "from"]
    )
    .expect("metric can be created");
}

/// `register_custom_metrics` registers custom metrics to the registry.
/// It have to be called before you plan to serve `/metrics` route.
pub fn register_custom_metrics() {
    REGISTRY
        .register(Box::new(ERRORS_COUNTER.clone()))
        .expect("collector can be registered");

    REGISTRY
        .register(Box::new(CHALLENGE_COUNTER.clone()))
        .expect("collector can be registered");

    REGISTRY
        .register(Box::new(LAST_SCANNED_BLOCK_GAUGE.clone()))
        .expect("collector can be registered");
}

// pub async fn metrics_handler() -> Result<impl Reply, Rejection> {
//     use prometheus::Encoder;
//     let encoder = prometheus::TextEncoder::new();

//     let mut buffer = Vec::new();
//     if let Err(e) = encoder.encode(&REGISTRY.gather(), &mut buffer) {
//         eprintln!("could not encode custom metrics: {}", e);
//     };
//     let mut res = match String::from_utf8(buffer.clone()) {
//         Ok(v) => v,
//         Err(e) => {
//             eprintln!("custom metrics could not be from_utf8'd: {}", e);
//             String::default()
//         }
//     };
//     buffer.clear();

//     let mut buffer = Vec::new();
//     if let Err(e) = encoder.encode(&prometheus::gather(), &mut buffer) {
//         eprintln!("could not encode prometheus metrics: {}", e);
//     };
//     let res_custom = match String::from_utf8(buffer.clone()) {
//         Ok(v) => v,
//         Err(e) => {
//             eprintln!("prometheus metrics could not be from_utf8'd: {}", e);
//             String::default()
//         }
//     };
//     buffer.clear();

//     res.push_str(&res_custom);
//     Ok(res)
// }
