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

//! Parameter optimization adapter for Mattia's market maker strategy.

use anyhow::Result;
use nautilus_model::types::Quantity;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use rustuna_core::trial::Trial;
use serde::Serialize;

use crate::optimizer::{
    objective::{run_backtest, BacktestEnv, BacktestMetrics},
    param::{suggest_float, suggest_int, SearchSpace},
    Optimizable,
};
use crate::strategy::mmm::{config::MattiasMarketMakerConfig, strategy::MattiasMarketMaker};

/// Strategy parameters sampled by the optimizer.
///
/// Greek-letter config fields are represented here as `Φ_0`/`Q_max` in quantity
/// terms and `Δ_0`/`Δ_μ`/`β` as decimal terms.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MattiasParams {
    pub phi_0: f64,
    pub phi_n: i64,
    pub q_max: f64,
    pub delta_0: f64,
    pub delta_mu: f64,
    pub beta: f64,
}

impl MattiasParams {
    pub fn to_config(&self, env: &BacktestEnv) -> Result<MattiasMarketMakerConfig> {
        let to_quantity = |v: f64| -> Result<Quantity> {
            Ok(Quantity::from(&format!("{v:.4}")))
        };
        let to_decimal = |v: f64| -> Result<Decimal> {
            Decimal::from_f64(v)
                .ok_or_else(|| anyhow::anyhow!("param value {v} cannot be represented as Decimal"))
        };
        Ok(MattiasMarketMakerConfig::builder()
            .instrument_id(env.instrument_id)
            .catalog_path(env.catalog_path.clone())
            .Φ_0(to_quantity(self.phi_0)?)
            .Φ_n(self.phi_n as u8)
            .Q_max(to_quantity(self.q_max)?)
            .Δ_0(to_decimal(self.delta_0)?)
            .Δ_μ(to_decimal(self.delta_mu)?)
            .β(to_decimal(self.beta)?)
            .build())
    }
}

/// Mattia's market maker optimization adapter.
#[derive(Debug, Clone, Default)]
pub struct MmOptimizable;

impl Optimizable for MmOptimizable {
    type Params = MattiasParams;

    fn strategy_name(&self) -> &'static str {
        "mmm"
    }

    fn known_params(&self) -> &'static [&'static str] {
        &["phi_0", "phi_n", "q_max", "delta_0", "delta_mu", "beta"]
    }

    fn suggest(&self, trial: &mut Trial, space: &SearchSpace) -> Result<Self::Params> {
        Ok(MattiasParams {
            phi_0: suggest_float(trial, space, "phi_0")?,
            phi_n: suggest_int(trial, space, "phi_n")?,
            q_max: suggest_float(trial, space, "q_max")?,
            delta_0: suggest_float(trial, space, "delta_0")?,
            delta_mu: suggest_float(trial, space, "delta_mu")?,
            beta: suggest_float(trial, space, "beta")?,
        })
    }

    fn evaluate(&self, params: &Self::Params, env: &BacktestEnv) -> Result<BacktestMetrics> {
        if params.q_max < params.phi_0 {
            anyhow::bail!("q_max below phi_0");
        }
        let config = params.to_config(env)?;
        run_backtest(env, MattiasMarketMaker::new(&config))
    }

    fn describe(&self, params: &Self::Params) -> String {
        format!(
            "phi_0={:.4} phi_n={} q_max={:.4} delta_0={:.4} delta_mu={:.4} beta={:.4}",
            params.phi_0, params.phi_n, params.q_max, params.delta_0, params.delta_mu, params.beta,
        )
    }

    fn to_fragment(&self, params: &Self::Params, section: &str) -> String {
        format!(
            "# best params from optimization\n[{section}]\n\"Φ_0\" = \"{:.4}\"\n\"Φ_n\" = {}\nQ_max = \"{:.4}\"\n\"Δ_0\" = \"{:.4}\"\n\"Δ_μ\" = \"{:.4}\"\n\"β\" = \"{:.4}\"\n",
            params.phi_0,
            params.phi_n,
            params.q_max,
            params.delta_0,
            params.delta_mu,
            params.beta,
        )
    }
}
