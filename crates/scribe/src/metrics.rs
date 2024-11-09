//  Copyright (C) 2021-2023 Chronicle Labs, Inc.
//
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU Affero General Public License as
//  published by the Free Software Foundation, either version 3 of the
//  License, or (at your option) any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU Affero General Public License for more details.
//
//  You should have received a copy of the GNU Affero General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.

use alloy::primitives::{Address, B256};
use metrics::{counter, describe_counter, describe_gauge, gauge};

const LAST_SCANNED_BLOCK_GAUGE: &str = "challenger_last_scanned_block";
const ERRORS_COUNTER: &str = "challenger_errors_total";
const CHALLENGE_COUNTER: &str = "challenger_challenges_total";

/// `set_last_scanned_block` sets the last scanned block for given `address` and `from` account.
pub fn set_last_scanned_block(from: Address, block: i64) {
    let labels = [(("from"), format!("{:?}", from))];
    gauge!(LAST_SCANNED_BLOCK_GAUGE, &labels).set(block as f64);
}

/// `inc_errors_counter` increments the errors counter for given `address`, `from` account
pub fn inc_errors_counter(address: Address, error: &str) {
    // TODO: Use it...
    let labels = [
        ("address", format!("{:?}", address)),
        ("error", String::from(error)),
    ];
    counter!(ERRORS_COUNTER, &labels).increment(1);
}

/// `inc_challenge_counter` increments the challenges counter for given `address` and `tx` hash.
pub fn inc_challenge_counter(address: Address, tx: B256, flashbots: bool) {
    let labels = [
        ("address", format!("{:?}", address)),
        ("flashbots", format!("{:?}", flashbots)),
        ("tx", format!("{:?}", tx)),
    ];
    counter!(CHALLENGE_COUNTER, &labels).increment(1);
}

/// `register_custom_metrics` registers custom metrics to the registry.
/// It have to be called before you start processing & if you plans to serve `/metrics` route.
pub fn describe() {
    describe_counter!(
        ERRORS_COUNTER,
        "Counts different errors in challenger process by given address"
    );
    describe_counter!(
        CHALLENGE_COUNTER,
        "Counts happened challenges for given address and from account"
    );
    describe_gauge!(
        LAST_SCANNED_BLOCK_GAUGE,
        "Keeps track of last scanned block for given address and from account"
    );
}
