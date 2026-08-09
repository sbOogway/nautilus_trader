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

//! `optimizer` - parameter optimization for strategy configurations.
//!
//! Runs a Rustuna-backed hyperparameter search (TPE / random / NSGA-II) over the
//! search space declared in a strategy's `[<strategy>.optimize]` section of
//! `config.toml`. Each trial runs a full backtest over the configured train window
//! and is scored by SQN and Sharpe; the best candidates are then re-scored on the
//! out-of-sample window.
//!
//! Every study and trial is persisted to a SQLite database (`db_path`, default
//! `optimizer.sqlite3`) in an Optuna-compatible schema (`studies`, `trials`,
//! `trial_params`, `trial_values`). Each run gets a unique timestamped study name
//! unless `study_name` is configured; re-running with an explicit name resumes the
//! existing study, appending new trials to its history.
//!
//! # Config
//!
//! The strategy to optimize is resolved from `--strategy`, or inferred when exactly
//! one strategy has an `[<strategy>.optimize]` section. Search parameters are declared
//! as `type = "int" | "float"` ranges:
//!
//! ```toml
//! [grid_mm.optimize]
//! trials = 200
//! sampler = "tpe"
//! seed = 42
//! db_path = "optimizer.sqlite3"
//! train_start = "2026-08-01"
//! train_end = "2026-08-07"
//! oos_start = "2026-08-08"
//! oos_end = "2026-08-09"
//! json_out = "study.json"
//! best_params_out = "best.toml"
//!
//! [grid_mm.optimize.params.num_levels]
//! type = "int"
//! min = 1
//! max = 20
//! ```
//!
//! # Usage
//!
//! Run a study from the config defaults:
//!
//! ```text
//! cargo run --bin optimizer -- --config config.toml
//! ```
//!
//! Explicitly pick a strategy when multiple have an optimize section, and override
//! individual settings from the CLI:
//!
//! ```text
//! cargo run --bin optimizer -- --strategy mmm --trials 100 --sampler random --seed 7 --db study.sqlite3
//! ```
//!
//! Inspect persisted runs directly in the database:
//!
//! ```text
//! sqlite3 optimizer.sqlite3 "SELECT study_name FROM studies"
//! sqlite3 optimizer.sqlite3 \
//!   "SELECT t.number, tp.param_name, tp.param_value, tv.value \
//!    FROM trials t JOIN trial_params tp ON tp.trial_id=t.id \
//!    JOIN trial_values tv ON tv.trial_id=t.id"
//! ```

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use nautilus_bin::config::{Config, OptimizeTomlConfig};
use nautilus_bin::optimizer::{
    pareto_front, print_trials, top_n_scored, write_study_json, BacktestEnv, Optimizable,
    RustunaOptimizer, SamplerKind, SearchSpace,
};
use nautilus_bin::strategy::grid_mm::optimize::GridMmOptimizable;
use nautilus_bin::strategy::mmm::optimize::MmOptimizable;
use nautilus_model::identifiers::{AccountId, InstrumentId};

#[derive(Parser, Debug)]
#[command(name = "optimizer", version = "...", about = "Optimize strategy parameters")]
struct Opts {
    /// Path to the config.toml file.
    #[arg(long, default_value = "config.toml")]
    config: String,
    /// Strategy to optimize: grid_mm | mmm. Inferred when only one has an optimize section.
    #[arg(long)]
    strategy: Option<String>,
    /// Override the number of backtest trials.
    #[arg(long)]
    trials: Option<usize>,
    /// Override the random seed.
    #[arg(long)]
    seed: Option<u64>,
    /// Override the search algorithm: tpe | random | nsgaii.
    #[arg(long)]
    sampler: Option<String>,
    /// Override the study name.
    #[arg(long)]
    study_name: Option<String>,
    /// Override the SQLite database file for persisting studies and trials.
    #[arg(long)]
    db: Option<String>,
    /// Override the output JSON file.
    #[arg(long)]
    json: Option<String>,
    /// Override the best-params output file.
    #[arg(long)]
    best_params_out: Option<String>,
}

fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0).unwrap(),
        Utc,
    ))
}

fn required_date(arg: Option<&str>, name: &str) -> Result<DateTime<Utc>> {
    arg.map(parse_date)
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("optimize config missing required `{name}` (YYYY-MM-DD)"))
}

fn fmt_date(d: DateTime<Utc>) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Resolves the strategy to optimize: an explicit `--strategy` wins; otherwise the
/// strategy whose config carries an `[<strategy>.optimize]` section.
fn resolve_strategy(flag: Option<&str>, cfg: &Config) -> Result<String> {
    if let Some(s) = flag {
        return match s {
            "grid_mm" | "mmm" => Ok(s.to_string()),
            other => bail!("unknown --strategy `{other}` (expected grid_mm|mmm)"),
        };
    }
    let mut found = Vec::new();
    if cfg
        .grid_mm
        .as_ref()
        .and_then(|g| g.optimize.as_ref())
        .is_some()
    {
        found.push("grid_mm");
    }
    if cfg.mmm.as_ref().and_then(|m| m.optimize.as_ref()).is_some() {
        found.push("mmm");
    }
    match found.as_slice() {
        [s] => Ok((*s).to_string()),
        [] => bail!(
            "no [<strategy>.optimize] section found in config.toml (pass --strategy to select)"
        ),
        _ => bail!(
            "multiple [<strategy>.optimize] sections found in config.toml (pass --strategy to select)"
        ),
    }
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    nautilus_common::logging::ensure_logging_initialized();

    let opts = Opts::parse();
    let cfg = Config::load(opts.config.clone())?;

    match resolve_strategy(opts.strategy.as_deref(), &cfg)?.as_str() {
        "grid_mm" => {
            let grid_cfg = cfg
                .grid_mm
                .as_ref()
                .context("config.toml missing [grid_mm] section")?;
            let opt_cfg = grid_cfg
                .optimize
                .as_ref()
                .context("config.toml missing [grid_mm.optimize] section")?;
            let env = BacktestEnv::builder()
                .catalog_path(grid_cfg.path.clone())
                .instrument_id(InstrumentId::from(grid_cfg.instrument_id.as_str()))
                .account_id(AccountId::from(opt_cfg.account_id.as_str()))
                .start(required_date(opt_cfg.train_start.as_deref(), "train_start")?)
                .end(required_date(opt_cfg.train_end.as_deref(), "train_end")?)
                .snapshot_interval_ms(opt_cfg.snapshot_interval_ms)
                .build();
            let adapter = GridMmOptimizable::new(grid_cfg.on_cancel_resubmit);
            run_strategy_optimize(&adapter, opt_cfg, &env, &opts)?;
        }
        "mmm" => {
            let mmm_cfg = cfg
                .mmm
                .as_ref()
                .context("config.toml missing [mmm] section")?;
            let opt_cfg = mmm_cfg
                .optimize
                .as_ref()
                .context("config.toml missing [mmm.optimize] section")?;
            let env = BacktestEnv::builder()
                .catalog_path(mmm_cfg.path.clone())
                .instrument_id(InstrumentId::from(mmm_cfg.instrument_id.as_str()))
                .account_id(AccountId::from(opt_cfg.account_id.as_str()))
                .start(required_date(opt_cfg.train_start.as_deref(), "train_start")?)
                .end(required_date(opt_cfg.train_end.as_deref(), "train_end")?)
                .snapshot_interval_ms(opt_cfg.snapshot_interval_ms)
                .build();
            let adapter = MmOptimizable;
            run_strategy_optimize(&adapter, opt_cfg, &env, &opts)?;
        }
        other => bail!("unknown --strategy `{other}` (expected grid_mm|mmm)"),
    }

    Ok(())
}

/// Runs one optimization study using a strategy's `[<strategy>.optimize]` config.
fn run_strategy_optimize<T: Optimizable>(
    adapter: &T,
    opt_cfg: &OptimizeTomlConfig,
    env: &BacktestEnv,
    opts: &Opts,
) -> Result<()> {
    let sampler = opts.sampler.as_deref().unwrap_or(&opt_cfg.sampler);
    let kind = SamplerKind::from_str(sampler)?;
    let trials = opts.trials.unwrap_or(opt_cfg.trials);
    let seed = opts.seed.or(opt_cfg.seed);
    // A unique timestamped name is used by default so each run is stored as its own study.
    // An explicit name either creates a fresh study or resumes the existing one.
    let study_name = opts
        .study_name
        .clone()
        .or_else(|| opt_cfg.study_name.clone())
        .unwrap_or_else(|| {
            let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            format!("{}-{sampler}-{ts}", adapter.strategy_name())
        });
    let db_path = opts.db.clone().unwrap_or_else(|| opt_cfg.db_path.clone());
    let json_out = opts.json.clone().unwrap_or_else(|| opt_cfg.json_out.clone());
    let best_params_out = opts
        .best_params_out
        .clone()
        .or_else(|| opt_cfg.best_params_out.clone());

    let oos_start = opt_cfg.oos_start.as_deref().map(parse_date).transpose()?;
    let oos_end = opt_cfg.oos_end.as_deref().map(parse_date).transpose()?;

    let space = opt_cfg.search_space()?;
    run_study(
        adapter,
        &space,
        env,
        &kind,
        seed,
        trials,
        &study_name,
        &db_path,
        &json_out,
        best_params_out.as_deref(),
        oos_start,
        oos_end,
        opt_cfg.weight_obj0,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_study<T: Optimizable>(
    adapter: &T,
    space: &SearchSpace,
    env: &BacktestEnv,
    kind: &SamplerKind,
    seed: Option<u64>,
    trials: usize,
    study_name: &str,
    db_path: &str,
    json_out: &str,
    best_params_out: Option<&str>,
    oos_start: Option<DateTime<Utc>>,
    oos_end: Option<DateTime<Utc>>,
    weight_obj0: f64,
) -> Result<()> {
    space.validate(adapter.known_params())?;

    log::info!(
        "study={study_name} strategy={} sampler={kind:?} seed={seed:?} trials={trials}",
        adapter.strategy_name()
    );
    log::info!(
        "train window {}..{}",
        fmt_date(env.start),
        fmt_date(env.end)
    );
    log::info!("study database: {db_path}");

    let optimizer = RustunaOptimizer::new(*kind, seed);
    let t0 = std::time::Instant::now();
    let records = optimizer.optimize(adapter, study_name, trials, space, env, db_path)?;
    log::info!(
        "completed {} trials in {:.1}s",
        records.len(),
        t0.elapsed().as_secs_f64()
    );

    let sampler_name = format!("{kind:?}").to_lowercase();
    write_study_json(json_out, study_name, &sampler_name, seed, &records)?;
    log::info!("study written to {json_out}");

    let pareto = pareto_front(&records);
    print_trials("Pareto front (train window)", &pareto, |p| {
        adapter.describe(p)
    });
    println!("Pareto front size: {}", pareto.len());

    let top = top_n_scored(&records, pareto.len().min(100), weight_obj0);
    print_trials("Top candidates by weighted score", &top, |p| {
        adapter.describe(p)
    });

    // Re-score the winners on the out-of-sample window.
    if let (Some(oos_start), Some(oos_end)) = (oos_start, oos_end) {
        if oos_end > env.end || oos_start > env.end {
            log::info!(
                "oos window {} .. {}",
                fmt_date(oos_start),
                fmt_date(oos_end)
            );
            let oos_env = env.with_window(oos_start, oos_end);
            println!("\nTop candidates on OOS window:");
            for r in &top {
                match adapter.evaluate(&r.params, &oos_env) {
                    Ok(m) => println!(
                        "  trial {} | sqn={:.2} mdd={:.1}% sharpe={:.2} trades={} | {}",
                        r.number,
                        m.sqn,
                        m.max_drawdown * 100.0,
                        m.sharpe_252.unwrap_or(f64::NAN),
                        m.total_orders,
                        adapter.describe(&r.params),
                    ),
                    Err(e) => println!("  trial {} OOS backtest failed: {e}", r.number),
                }
            }
        } else {
            log::warn!(
                "oos window {}..{} does not extend beyond the train window; skipping OOS",
                fmt_date(oos_start),
                fmt_date(oos_end)
            );
        }
    }

    if let (Some(path), Some(best)) = (best_params_out, top.first()) {
        let fragment = adapter.to_fragment(&best.params, adapter.strategy_name());
        std::fs::write(path, fragment)?;
        log::info!("best params written to {path}");
    }

    Ok(())
}
