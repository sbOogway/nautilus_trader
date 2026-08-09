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
//! The search space, sampler, trial budget and train/OOS windows are driven by the
//! `[optimize]` section of `config.toml`; CLI flags override individual values.

use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use nautilus_bin::config::Config;
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
        .ok_or_else(|| anyhow::anyhow!("[optimize] missing required `{name}` (YYYY-MM-DD)"))
}

fn fmt_date(d: DateTime<Utc>) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    nautilus_common::logging::ensure_logging_initialized();

    let opts = Opts::parse();
    let cfg = Config::load(opts.config.clone())?;
    let opt_cfg = cfg
        .optimize
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("config.toml missing [optimize] section"))?;

    let kind = SamplerKind::from_str(&opt_cfg.sampler)?;
    let trials = opts.trials.unwrap_or(opt_cfg.trials);
    let seed = opts.seed.or(opt_cfg.seed);
    let study_name = opts
        .study_name
        .clone()
        .or_else(|| opt_cfg.study_name.clone())
        .unwrap_or_else(|| format!("{}-{}", opt_cfg.strategy, opt_cfg.sampler));
    let json_out = opts.json.unwrap_or_else(|| opt_cfg.json_out.clone());
    let best_params_out = opts
        .best_params_out
        .or_else(|| opt_cfg.best_params_out.clone());

    let train_start = required_date(opt_cfg.train_start.as_deref(), "train_start")?;
    let train_end = required_date(opt_cfg.train_end.as_deref(), "train_end")?;
    let oos_start = opt_cfg.oos_start.as_deref().map(parse_date).transpose()?;
    let oos_end = opt_cfg.oos_end.as_deref().map(parse_date).transpose()?;

    let space = opt_cfg.search_space()?;
    let account_id = AccountId::from(opt_cfg.account_id.as_str());

    match opt_cfg.strategy.as_str() {
        "grid_mm" => {
            let grid_cfg = cfg
                .grid_mm
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("config.toml missing [grid_mm] section"))?;
            let env = BacktestEnv::builder()
                .catalog_path(grid_cfg.path.clone())
                .instrument_id(InstrumentId::from(grid_cfg.instrument_id.as_str()))
                .account_id(account_id)
                .start(train_start)
                .end(train_end)
                .snapshot_interval_ms(opt_cfg.snapshot_interval_ms)
                .build();
            let adapter = GridMmOptimizable::new(
                grid_cfg.expire_time_secs,
                grid_cfg.on_cancel_resubmit,
            );
            run_study(
                &adapter,
                &space,
                &env,
                &kind,
                seed,
                trials,
                &study_name,
                &json_out,
                best_params_out.as_deref(),
                oos_start,
                oos_end,
                opt_cfg.weight_obj0,
            )?;
        }
        "mmm" => {
            let mmm_cfg = cfg
                .mmm
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("config.toml missing [mmm] section"))?;
            let env = BacktestEnv::builder()
                .catalog_path(mmm_cfg.path.clone())
                .instrument_id(InstrumentId::from(mmm_cfg.instrument_id.as_str()))
                .account_id(account_id)
                .start(train_start)
                .end(train_end)
                .snapshot_interval_ms(opt_cfg.snapshot_interval_ms)
                .build();
            let adapter = MmOptimizable;
            run_study(
                &adapter,
                &space,
                &env,
                &kind,
                seed,
                trials,
                &study_name,
                &json_out,
                best_params_out.as_deref(),
                oos_start,
                oos_end,
                opt_cfg.weight_obj0,
            )?;
        }
        other => bail!("unknown optimize.strategy `{other}` (expected grid_mm|mmm)"),
    }

    Ok(())
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

    let optimizer = RustunaOptimizer::new(*kind, seed);
    let t0 = std::time::Instant::now();
    let records = optimizer.optimize(adapter, study_name, trials, space, env)?;
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
