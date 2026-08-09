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

//! Sampler abstractions around the Rustuna optimization framework.

use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use rustuna_core::{
    sampler::{RandomSampler, Sampler},
    storage::Storage,
    study::{create_study, Direction, Study},
    trial::Trial,
    Error as RustunaError, ErrorKind,
};
use rustuna_sampler::{
    nsgaii::NSGAIISampler,
    tpe::{TpeConfig, TpeSampler},
};
use rustuna_storage::{
    cache::CachedStorage,
    sqlite3::SQLite3Storage,
};
use serde::Serialize;

use super::{
    objective::{to_objectives, BacktestEnv, BacktestMetrics},
    param::SearchSpace,
    report::RecordedTrial,
};

/// Supported search algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerKind {
    Tpe,
    Random,
    NsgaIi,
}

impl FromStr for SamplerKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "tpe" => Ok(Self::Tpe),
            "random" => Ok(Self::Random),
            "nsgaii" => Ok(Self::NsgaIi),
            other => anyhow::bail!("unknown sampler `{other}` (expected tpe|random|nsgaii)"),
        }
    }
}

/// Strategy adapter contract for parameter optimization.
///
/// Implementations define how a strategy's parameters are sampled from the search
/// space, mapped onto a runnable backtest, and rendered for reporting.
pub trait Optimizable {
    /// The strategy-specific parameter set produced by each trial.
    type Params: Clone + Serialize + 'static;

    /// Strategy name, used for dispatch and study naming.
    fn strategy_name(&self) -> &'static str;

    /// The parameter names this strategy supports.
    fn known_params(&self) -> &'static [&'static str];

    /// Samples a full parameter set from a Rustuna trial using the given search space.
    ///
    /// Errors are treated as configuration errors and abort the study.
    fn suggest(&self, trial: &mut Trial, space: &SearchSpace) -> Result<Self::Params>;

    /// Runs a backtest for `params` and returns the outcome metrics.
    fn evaluate(&self, params: &Self::Params, env: &BacktestEnv) -> Result<BacktestMetrics>;

    /// Renders a human-readable description of a parameter set.
    fn describe(&self, params: &Self::Params) -> String;

    /// Renders a TOML fragment with the given parameters under `section`.
    fn to_fragment(&self, params: &Self::Params, section: &str) -> String;
}

/// Rustuna-backed optimizer (TPE / random / NSGA-II search algorithms).
#[derive(Debug, Clone)]
pub struct RustunaOptimizer {
    kind: SamplerKind,
    seed: Option<u64>,
}

impl RustunaOptimizer {
    pub fn new(kind: SamplerKind, seed: Option<u64>) -> Self {
        Self { kind, seed }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_study<S, T>(
        &self,
        sampler: S,
        adapter: &T,
        study_name: &str,
        n_trials: usize,
        space: &SearchSpace,
        env: &BacktestEnv,
        db_path: &str,
        records: &mut Vec<RecordedTrial<T::Params>>,
    ) -> Result<()>
    where
        S: Sampler + Send + 'static,
        T: Optimizable,
    {
        // Persist every study and trial to SQLite (Optuna-compatible schema). A study with the
        // given name is resumed when it already exists, appending new trials to the history.
        let backend = SQLite3Storage::new(db_path)
            .map_err(|e| anyhow::anyhow!("rustuna: failed to open study database `{db_path}`: {e}"))?;
        backend
            .create_database()
            .map_err(|e| anyhow::anyhow!("rustuna: failed to initialize study database: {e}"))?;

        let mut cached = CachedStorage::new(Box::new(backend));
        let resume = cached
            .get_studies()
            .map_err(|e| anyhow::anyhow!("rustuna: failed to query studies: {e}"))?
            .iter()
            .any(|s| s.name == study_name);

        let directions = vec![Direction::Minimize, Direction::Minimize];
        let study = if resume {
            let storage: Arc<RwLock<dyn Storage>> = Arc::new(RwLock::new(cached));
            let sampler: Arc<Mutex<dyn Sampler>> = Arc::new(Mutex::new(sampler));
            Study::from_name(study_name.to_string(), storage, sampler)
                .map_err(|e| anyhow::anyhow!("rustuna: failed to load study `{study_name}`: {e}"))?
        } else {
            create_study(study_name, cached, sampler, directions)
                .map_err(|e| anyhow::anyhow!("rustuna: failed to create study: {e}"))?
        };

        study
            .optimize(
                |mut trial| {
                    let params = match adapter.suggest(&mut trial, space) {
                        Ok(params) => params,
                        Err(e) => {
                            return Err(RustunaError::with_reason(
                                ErrorKind::ObjectiveError,
                                e.to_string(),
                            ));
                        }
                    };

                    match adapter.evaluate(&params, env) {
                        Ok(metrics) => {
                            let [obj0, obj1] = to_objectives(&metrics);
                            records.push(RecordedTrial {
                                number: trial.number,
                                params,
                                metrics,
                                obj0,
                                obj1,
                            });
                            Ok(vec![obj0, obj1])
                        }
                        // A failing trial must not abort the study: report infinite
                        // objectives so the sampler learns nothing from it.
                        Err(_) => {
                            records.push(RecordedTrial {
                                number: trial.number,
                                params,
                                metrics: BacktestMetrics {
                                    sqn: f64::NAN,
                                    max_drawdown: 0.0,
                                    sharpe_252: None,
                                    n_returns: 0,
                                    total_orders: 0,
                                    total_positions: 0,
                                },
                                obj0: f64::INFINITY,
                                obj1: f64::INFINITY,
                            });
                            Ok(vec![f64::INFINITY, f64::INFINITY])
                        }
                    }
                },
                n_trials,
            )
            .map_err(|e| anyhow::anyhow!("rustuna: optimization failed: {e}"))?;

        Ok(())
    }

    /// Runs `n_trials` backtests over `space`, returning per-trial records.
    ///
    /// Studies and trials are persisted to the SQLite database at `db_path`.
    pub fn optimize<T: Optimizable>(
        &self,
        adapter: &T,
        study_name: &str,
        n_trials: usize,
        space: &SearchSpace,
        env: &BacktestEnv,
        db_path: &str,
    ) -> Result<Vec<RecordedTrial<T::Params>>> {
        let mut records = Vec::new();

        match (self.kind, self.seed) {
            (SamplerKind::Tpe, Some(seed)) => self.run_study(
                TpeSampler::from_config(TpeConfig {
                    n_startup_trials: 10,
                    seed: Some(seed),
                    multivariate: None,
                }),
                adapter,
                study_name,
                n_trials,
                space,
                env,
                db_path,
                &mut records,
            )?,
            (SamplerKind::Tpe, None) => self.run_study(
                TpeSampler::new(),
                adapter,
                study_name,
                n_trials,
                space,
                env,
                db_path,
                &mut records,
            )?,
            (SamplerKind::Random, Some(seed)) => self.run_study(
                RandomSampler::seed_from_u64(seed),
                adapter,
                study_name,
                n_trials,
                space,
                env,
                db_path,
                &mut records,
            )?,
            (SamplerKind::Random, None) => self.run_study(
                RandomSampler::new(),
                adapter,
                study_name,
                n_trials,
                space,
                env,
                db_path,
                &mut records,
            )?,
            (SamplerKind::NsgaIi, Some(seed)) => self.run_study(
                NSGAIISampler::seed_from_u64(seed, 50, Some(0.2), 0.8, 0.5),
                adapter,
                study_name,
                n_trials,
                space,
                env,
                db_path,
                &mut records,
            )?,
            (SamplerKind::NsgaIi, None) => self.run_study(
                NSGAIISampler::new(50, Some(0.2), 0.8, 0.5),
                adapter,
                study_name,
                n_trials,
                space,
                env,
                db_path,
                &mut records,
            )?,
        }

        records.sort_by_key(|r| r.number);
        Ok(records)
    }
}
