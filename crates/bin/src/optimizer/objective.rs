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

//! Objective function plumbing: shared backtest runner and outcome metrics.

use std::fmt::Debug;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use nautilus_backtest::{
    config::{
        BacktestDataConfig, BacktestEngineConfig, BacktestRunConfig, BacktestVenueConfig,
        NautilusDataType,
    },
    node::BacktestNode,
};
use nautilus_common::{actor::DataActorNative, component::Component};
use nautilus_model::{
    enums::{AccountType, BookType, OmsType},
    identifiers::{AccountId, InstrumentId},
};
use nautilus_portfolio::config::PortfolioConfig;
use nautilus_trading::strategy::{Strategy, StrategyNative};
use serde::Serialize;

/// Penalty strength applied to max drawdown in the SQN objective.
pub const SQN_DD_PENALTY: f64 = 2.0;

/// Interval (ms) between fine-grained portfolio equity snapshots used for the SQN objective.
///
/// The default `PortfolioConfig` only records daily equity samples (UTC midnight plus
/// registration/shutdown), which makes SQN statistically meaningless for short backtest windows.
/// Sampling while the account holds a position gives a dense equity curve instead.
pub const DEFAULT_SNAPSHOT_INTERVAL_MS: u64 = 3_600_000;

/// Fixed environment of a backtest run, independent of the searched parameters.
#[derive(Debug, Clone, bon::Builder)]
pub struct BacktestEnv {
    pub catalog_path: String,
    pub instrument_id: InstrumentId,
    pub account_id: AccountId,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Interval (ms) between fine-grained portfolio equity snapshots for the SQN objective.
    #[builder(default = DEFAULT_SNAPSHOT_INTERVAL_MS)]
    pub snapshot_interval_ms: u64,
    // Venue configuration (fixed across trials).
    #[builder(default = "BYBIT".to_string())]
    pub venue_name: String,
    #[builder(default = OmsType::Hedging)]
    pub oms_type: OmsType,
    #[builder(default = AccountType::Margin)]
    pub account_type: AccountType,
    #[builder(default = BookType::L2_MBP)]
    pub book_type: BookType,
    #[builder(default = vec!["1_000 USDT".to_string()])]
    pub starting_balances: Vec<String>,
}

impl BacktestEnv {
    #[must_use]
    pub fn with_window(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        Self {
            catalog_path: self.catalog_path.clone(),
            instrument_id: self.instrument_id,
            account_id: self.account_id,
            start,
            end,
            snapshot_interval_ms: self.snapshot_interval_ms,
            venue_name: self.venue_name.clone(),
            oms_type: self.oms_type,
            account_type: self.account_type,
            book_type: self.book_type,
            starting_balances: self.starting_balances.clone(),
        }
    }
}

/// Outcome metrics of a single backtest run.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BacktestMetrics {
    pub sqn: f64,
    pub max_drawdown: f64,
    pub sharpe_252: Option<f64>,
    pub n_returns: usize,
    pub total_orders: usize,
    pub total_positions: usize,
}

/// Maps backtest metrics to the two minimized objectives.
/// objective[0]: -SQN / (1 + SQN_DD_PENALTY * |max drawdown|)
/// objective[1]: -Sharpe Ratio (252 days), annualized by nautilus analysis.
pub fn to_objectives(metrics: &BacktestMetrics) -> [f64; 2] {
    if metrics.sqn.is_nan()
        || metrics.sqn.is_infinite()
        || metrics.max_drawdown.is_nan()
        || metrics.n_returns == 0
    {
        return [f64::INFINITY, f64::INFINITY];
    }
    let obj0 = -metrics.sqn / (1.0 + SQN_DD_PENALTY * metrics.max_drawdown.abs());
    let obj1 = match metrics.sharpe_252 {
        Some(sharpe) if sharpe.is_finite() => -sharpe,
        _ => f64::INFINITY,
    };
    [obj0, obj1]
}

/// Runs a full backtest over `env.start..end` with the given strategy.
///
/// The venue, data and engine configuration is shared across all optimized strategies;
/// only the strategy itself varies per trial.
pub fn run_backtest<S>(env: &BacktestEnv, strategy: S) -> Result<BacktestMetrics>
where
    S: Strategy + StrategyNative + DataActorNative + Component + Debug + 'static,
{
    let venue = BacktestVenueConfig::builder()
        .name(&env.venue_name)
        .oms_type(env.oms_type)
        .account_type(env.account_type)
        .book_type(env.book_type)
        .starting_balances(env.starting_balances.clone())
        .build()?;

    let order_book = BacktestDataConfig::builder()
        .catalog_path(env.catalog_path.clone())
        .instrument_id(env.instrument_id)
        .start_time(env.start.into())
        .end_time(env.end.into())
        .data_type(NautilusDataType::OrderBookDelta)
        .optimize_file_loading(true)
        .build()?;

    let trades = BacktestDataConfig::builder()
        .catalog_path(env.catalog_path.clone())
        .instrument_id(env.instrument_id)
        .start_time(env.start.into())
        .end_time(env.end.into())
        .data_type(NautilusDataType::TradeTick)
        .optimize_file_loading(true)
        .build()?;

    let portfolio = PortfolioConfig::builder()
        .maybe_snapshot_interval_ms(Some(env.snapshot_interval_ms))
        .build()?;
    let engine = BacktestEngineConfig::builder()
        .bypass_logging(true)
        .portfolio(portfolio)
        .build();
    let run = BacktestRunConfig::builder()
        .id("grid-mm-optimize".to_string())
        .venues(vec![venue])
        .data(vec![order_book, trades])
        .chunk_size(1_000_000)
        .engine(engine)
        .build()?;

    let mut node = BacktestNode::new(vec![run]).context("failed to create backtest node")?;
    node.build().context("failed to build backtest node")?;
    {
        let engine = node
            .get_engine_mut("grid-mm-optimize")
            .context("backtest engine not found")?;
        engine
            .add_strategy(strategy)
            .context("failed to add strategy")?;
    }
    node.run().context("backtest run failed")?;

    let engine = node.get_engine_mut("grid-mm-optimize").unwrap();

    let snapshots = engine.kernel().portfolio.borrow().snapshots(&env.account_id);
    let equity: Vec<f64> = snapshots
        .iter()
        .filter_map(|s| s.total_equity.first().map(|m| m.as_f64()))
        .collect();

    let result = engine.get_result();

    let total_orders = result.total_orders;
    let total_positions = result.total_positions;

    let sharpe_252 = result
        .stats_returns
        .get("Sharpe Ratio (252 days)")
        .copied()
        .filter(|v| v.is_finite());

    let n_returns = equity.len().saturating_sub(1);
    if n_returns == 0 {
        return Ok(BacktestMetrics {
            sqn: f64::NAN,
            max_drawdown: 0.0,
            sharpe_252,
            n_returns: 0,
            total_orders,
            total_positions,
        });
    }

    let mut returns = Vec::with_capacity(n_returns);
    let mut running_max = equity[0];
    let mut max_drawdown = 0.0_f64;
    for w in equity.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        if prev <= 0.0 {
            continue;
        }
        returns.push(cur / prev - 1.0);
        running_max = running_max.max(cur);
        max_drawdown = max_drawdown.min(cur / running_max - 1.0);
    }

    let n = returns.len();
    if n == 0 {
        return Ok(BacktestMetrics {
            sqn: f64::NAN,
            max_drawdown,
            sharpe_252,
            n_returns: 0,
            total_orders,
            total_positions,
        });
    }

    let mean = returns.iter().sum::<f64>() / n as f64;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    let sqn = if variance > 0.0 {
        mean / variance.sqrt() * (n as f64).sqrt()
    } else {
        0.0
    };

    Ok(BacktestMetrics {
        sqn,
        max_drawdown,
        sharpe_252,
        n_returns: n,
        total_orders,
        total_positions,
    })
}
