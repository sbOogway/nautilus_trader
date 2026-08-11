// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Configuration for the VPIN market making strategy.

use nautilus_model::{
    identifiers::{InstrumentId, StrategyId},
    types::Quantity,
};
use nautilus_trading::StrategyConfig;

/// Configuration for the VPIN market making strategy.
///
/// Combines grid market making parameters with VPIN flow toxicity
/// parameters for dynamic spread adjustment and quote pulling.
#[derive(Debug, Clone, bon::Builder)]
pub struct VpinMarketMakerConfig {
    /// Base strategy configuration.
    #[builder(default = StrategyConfig {
        strategy_id: Some(StrategyId::from("VPIN_MM-001")),
        order_id_tag: Some("001".to_string()),
        ..Default::default()
    })]
    pub base: StrategyConfig,
    /// Instrument ID to trade.
    pub instrument_id: InstrumentId,
    /// Trade size per grid level. When `None` the strategy resolves it from
    /// the instrument's `min_quantity` during `on_start`.
    pub trade_size: Option<Quantity>,
    /// Number of price levels on each side (buy & sell).
    #[builder(default = 3)]
    pub num_levels: usize,
    /// Grid spacing in basis points of mid-price (geometric grid).
    /// E.g. `10` = 10 bps = 0.1%. Buy level N = mid × (1 - bps/10000)^N.
    #[builder(default = 10)]
    pub grid_step_bps: u32,
    /// How aggressively to shift the grid based on inventory.
    #[builder(default = 0.0)]
    pub skew_factor: f64,
    /// Hard cap on net exposure (long or short).
    pub max_position: Quantity,
    /// Minimum mid-price move in basis points before re-quoting.
    /// E.g. `5` = 5 bps = 0.05%.
    #[builder(default = 5)]
    pub requote_threshold_bps: u32,
    /// Optional order expiry in seconds. When set, orders use GTD
    /// time-in-force with `expire_time = now + expire_time_secs`.
    pub expire_time_secs: Option<u64>,
    /// When `true`, resubmit the full grid on the next timer tick after receiving
    /// an order cancel event.
    #[builder(default = false)]
    pub on_cancel_resubmit: bool,

    // VPIN-specific parameters

    /// Rolling window size for VPIN averaging (number of completed buckets).
    #[builder(default = 50)]
    pub vpin_window: usize,
    /// VPIN sensitivity for spread widening. Effective spread = grid_step_bps × (1 + alpha × VPIN).
    #[builder(default = 2.0)]
    pub vpin_spread_alpha: f64,
    /// VPIN threshold above which all quotes are pulled (toxic flow).
    #[builder(default = 0.7)]
    pub vpin_pull_threshold: f64,
    /// Target volume per bucket. When accumulated volume exceeds this,
    /// a new VPIN bucket is completed.
    #[builder(default = 1000.0)]
    pub bucket_target_volume: f64,
    /// Timer interval in seconds for grid re-evaluation.
    #[builder(default = 5)]
    pub timer_interval_secs: u64,
}
