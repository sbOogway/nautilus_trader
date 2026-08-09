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

use anyhow::{Context, Result};
use nautilus_common::enums::Environment;
use nautilus_model::types::Quantity;
use rust_decimal::Decimal;
use serde::{de, Deserialize, Deserializer};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use crate::optimizer::ParamKind;

#[derive(Debug, Deserialize)]
pub struct GridMarketMakerTomlConfig {
    pub exchange: String,
    pub trader_id: String,
    pub instrument_id: String,
    pub max_position: String,
    pub trade_size: String,
    #[serde(default = "default_num_levels")]
    pub num_levels: usize,
    #[serde(default = "default_grid_step_bps")]
    pub grid_step_bps: u32,
    #[serde(default)]
    pub skew_factor: f64,
    #[serde(default = "default_requote_threshold_bps")]
    pub requote_threshold_bps: u32,
    pub expire_time_secs: Option<u64>,
    #[serde(default)]
    pub on_cancel_resubmit: bool,
    #[serde(default = "default_recorder_path")]
    pub path: String,
    #[serde(deserialize_with = "deserialize_environment")]
    pub execution_environment: Environment,
}

#[derive(Debug, Deserialize)]
pub struct RecorderTomlConfig {
    pub exchange: String,
    pub trader_id: String,
    pub instrument_id: Vec<String>,
    #[serde(default = "default_recorder_path")]
    pub catalog_path: String,
    pub book_depth: usize,
    pub interval_parquet_dump_seconds: u64
}


#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
pub struct MattiasMarketMakerTomlConfig {
    pub exchange: String,
    pub trader_id: String,
    pub instrument_id: String,
    #[serde(default = "default_recorder_path")]
    pub path: String,
    pub Q_max: Quantity,
    pub Φ_0: Quantity,
    pub Φ_n: u8,
    pub Δ_0: Decimal,
    pub Δ_μ: Decimal,
    pub β: Decimal,
    
    /// execution environment. possible values are live and backtest
    #[serde(deserialize_with = "deserialize_environment")]
    pub execution_environment: Environment
}


#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(rename = "grid_mm")]
    pub grid_mm: Option<GridMarketMakerTomlConfig>,
    pub recorder: Option<RecorderTomlConfig>,
    pub mmm: Option<MattiasMarketMakerTomlConfig>,
    pub optimize: Option<OptimizeTomlConfig>,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config at {:?}", path.as_ref().display()))?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn load(path: String) -> Result<Self> {
        Self::from_file(path)
    }
}

fn deserialize_environment<'de, D>(deserializer: D) -> Result<Environment, D::Error>
where D: Deserializer<'de> {
    let s = String::deserialize(deserializer)?;
    Environment::from_str(&s).map_err(de::Error::custom)
}

#[derive(Debug, Deserialize)]
pub struct OptimizeTomlConfig {
    /// Strategy adapter to optimize: `grid_mm` or `mmm`.
    pub strategy: String,
    /// Number of backtest trials to run.
    #[serde(default = "default_trials")]
    pub trials: usize,
    /// Search algorithm: tpe | random | nsgaii.
    #[serde(default = "default_sampler")]
    pub sampler: String,
    /// Random seed for reproducible runs.
    pub seed: Option<u64>,
    /// Weight (0..1) of the first objective when ranking Pareto candidates.
    #[serde(default = "default_weight_obj0")]
    pub weight_obj0: f64,
    /// Study name (recorded in the output JSON).
    pub study_name: Option<String>,
    /// Train window start (YYYY-MM-DD, UTC).
    pub train_start: Option<String>,
    /// Train window end (YYYY-MM-DD, UTC).
    pub train_end: Option<String>,
    /// Out-of-sample window start (YYYY-MM-DD, UTC).
    pub oos_start: Option<String>,
    /// Out-of-sample window end (YYYY-MM-DD, UTC).
    pub oos_end: Option<String>,
    /// Output JSON file for the study results.
    #[serde(default = "default_json_out")]
    pub json_out: String,
    /// File to write the best parameter set as a TOML fragment.
    pub best_params_out: Option<String>,
    /// Interval (ms) between portfolio equity snapshots used for the SQN objective.
    #[serde(default = "default_snapshot_interval_ms")]
    pub snapshot_interval_ms: u64,
    /// Account ID used in the backtest venue.
    #[serde(default = "default_account_id")]
    pub account_id: String,
    /// Search space: named parameter ranges (`type = "int" | "float"`, min/max).
    #[serde(default)]
    pub params: HashMap<String, ParamKind>,
}

impl OptimizeTomlConfig {
    /// Validates the config and returns a `SearchSpace` built from `params`.
    pub fn search_space(&self) -> Result<crate::optimizer::SearchSpace> {
        if !(0.0..=1.0).contains(&self.weight_obj0) {
            anyhow::bail!("weight_obj0 must be within 0..1, got {}", self.weight_obj0);
        }
        let params = self
            .params
            .iter()
            .map(|(name, kind)| crate::optimizer::ParamSpec {
                name: name.clone(),
                kind: *kind,
            })
            .collect();
        Ok(crate::optimizer::SearchSpace { params })
    }
}

fn default_trials() -> usize {
    200
}

fn default_sampler() -> String {
    "tpe".into()
}

fn default_weight_obj0() -> f64 {
    0.5
}

fn default_json_out() -> String {
    "study.json".into()
}

fn default_snapshot_interval_ms() -> u64 {
    crate::optimizer::objective::DEFAULT_SNAPSHOT_INTERVAL_MS
}

fn default_account_id() -> String {
    "BYBIT-001".into()
}

fn default_num_levels() -> usize {
    3
}

fn default_grid_step_bps() -> u32 {
    10
}

fn default_requote_threshold_bps() -> u32 {
    5
}

fn default_recorder_path() -> String {
    "data/".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimize_section_deserializes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml");
        let cfg = Config::load(path.to_string()).expect("config.toml should parse");
        let opt = cfg.optimize.expect("[optimize] section present");
        assert_eq!(opt.strategy, "grid_mm");
        assert_eq!(opt.params.len(), 6);
        let space = opt.search_space().expect("search space valid");
        assert_eq!(space.params.len(), 6);
    }
}
