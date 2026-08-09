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

//! Parameter optimization adapter for the grid market making strategy.

use anyhow::{bail, Result};
use nautilus_model::{identifiers::InstrumentId, types::Quantity};
use rustuna_core::trial::Trial;
use serde::Serialize;

use crate::optimizer::{
    objective::{run_backtest, BacktestEnv, BacktestMetrics},
    param::{suggest_float, suggest_int, SearchSpace},
    Optimizable,
};
use crate::strategy::grid_mm::{
    config::GridMarketMakerConfig, strategy::GridMarketMaker,
};

/// Strategy parameters sampled by the optimizer.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct GridParams {
    pub num_levels: i64,
    pub grid_step_bps: i64,
    pub requote_threshold_bps: i64,
    pub skew_factor: f64,
    pub max_position: f64,
    pub trade_size: f64,
}

impl GridParams {
    pub fn to_config(
        &self,
        instrument_id: InstrumentId,
        expire_time_secs: Option<u64>,
        on_cancel_resubmit: bool,
    ) -> GridMarketMakerConfig {
        GridMarketMakerConfig::builder()
            .instrument_id(instrument_id)
            .max_position(Quantity::from(&format!("{:.4}", self.max_position)))
            .trade_size(Quantity::from(&format!("{:.4}", self.trade_size)))
            .num_levels(self.num_levels as usize)
            .grid_step_bps(self.grid_step_bps as u32)
            .skew_factor(self.skew_factor)
            .requote_threshold_bps(self.requote_threshold_bps as u32)
            .maybe_expire_time_secs(expire_time_secs)
            .on_cancel_resubmit(on_cancel_resubmit)
            .build()
    }
}

/// Grid market maker optimization adapter.
///
/// Holds the fixed strategy settings (not part of the search space) that are read from the
/// `[grid_mm]` config section.
#[derive(Debug, Clone)]
pub struct GridMmOptimizable {
    pub expire_time_secs: Option<u64>,
    pub on_cancel_resubmit: bool,
}

impl GridMmOptimizable {
    pub fn new(expire_time_secs: Option<u64>, on_cancel_resubmit: bool) -> Self {
        Self {
            expire_time_secs,
            on_cancel_resubmit,
        }
    }
}

impl Optimizable for GridMmOptimizable {
    type Params = GridParams;

    fn strategy_name(&self) -> &'static str {
        "grid_mm"
    }

    fn known_params(&self) -> &'static [&'static str] {
        &[
            "num_levels",
            "grid_step_bps",
            "requote_threshold_bps",
            "skew_factor",
            "max_position",
            "trade_size",
        ]
    }

    fn suggest(&self, trial: &mut Trial, space: &SearchSpace) -> Result<Self::Params> {
        Ok(GridParams {
            num_levels: suggest_int(trial, space, "num_levels")?,
            grid_step_bps: suggest_int(trial, space, "grid_step_bps")?,
            requote_threshold_bps: suggest_int(trial, space, "requote_threshold_bps")?,
            skew_factor: suggest_float(trial, space, "skew_factor")?,
            max_position: suggest_float(trial, space, "max_position")?,
            trade_size: suggest_float(trial, space, "trade_size")?,
        })
    }

    fn evaluate(&self, params: &Self::Params, env: &BacktestEnv) -> Result<BacktestMetrics> {
        if params.max_position < params.trade_size {
            bail!("max_position below trade_size");
        }
        let config =
            params.to_config(env.instrument_id, self.expire_time_secs, self.on_cancel_resubmit);
        run_backtest(env, GridMarketMaker::new(config))
    }

    fn describe(&self, params: &Self::Params) -> String {
        format!(
            "levels={} step={}bps requote={}bps skew={:.2} maxpos={:.4} size={:.4}",
            params.num_levels,
            params.grid_step_bps,
            params.requote_threshold_bps,
            params.skew_factor,
            params.max_position,
            params.trade_size,
        )
    }

    fn to_fragment(&self, params: &Self::Params, section: &str) -> String {
        format!(
            "# best params from optimization\n[{section}]\nnum_levels = {}\ngrid_step_bps = {}\nrequote_threshold_bps = {}\nskew_factor = {}\nmax_position = \"{:.4}\"\ntrade_size = \"{:.4}\"\n",
            params.num_levels,
            params.grid_step_bps,
            params.requote_threshold_bps,
            params.skew_factor,
            params.max_position,
            params.trade_size,
        )
    }
}
