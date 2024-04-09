use ethers::types::{Address, H256};
use eyre::{Context, Result};
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

/// `set_last_scanned_block` sets the last scanned block for given `address` and `from` account.
pub fn set_last_scanned_block(address: Address, from: Address, block: i64) {
    LAST_SCANNED_BLOCK_GAUGE
        .with_label_values(&[&format!("{:?}", address), &format!("{:?}", from)])
        .set(block);
}

/// `inc_challenges_counter` increments the errors counter for given `address`, `from` account
pub fn inc_errors_counter(address: Address, from: Address, error: &str) {
    ERRORS_COUNTER
        .with_label_values(&[&format!("{:?}", address), &format!("{:?}", from), error])
        .inc();
}

/// `inc_challenge_counter` increments the challenges counter for given `address`, `from` account and tx hash.
pub fn inc_challenge_counter(address: Address, from: Address, tx: H256) {
    CHALLENGE_COUNTER
        .with_label_values(&[
            &format!("{:?}", address),
            &format!("{:?}", from),
            &format!("{:?}", tx),
        ])
        .inc();
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

pub fn as_encoded_string() -> Result<String> {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();

    // Collect and encode custom metrics from `REGISTRY`
    let mut buffer = Vec::new();
    encoder
        .encode(&REGISTRY.gather(), &mut buffer)
        .wrap_err("Failed to encode REGISTRY metrics")?;

    let mut res = String::from_utf8(buffer.clone())
        .wrap_err("Failed to convert REGISTRY metrics from utf8")?;
    buffer.clear();

    // Collect and encode prometheus metrics from `prometheus::gather()`
    let mut buffer = Vec::new();
    encoder
        .encode(&prometheus::gather(), &mut buffer)
        .wrap_err("Failed to encode prometheus metrics")?;

    let res_custom = String::from_utf8(buffer.clone())
        .wrap_err("Failed to convert prometheus metrics from utf8")?;
    buffer.clear();

    res.push_str(&res_custom);
    Ok(res)
}
