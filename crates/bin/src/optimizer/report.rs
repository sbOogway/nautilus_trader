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

//! Reporting of study results: Pareto front, ranking, JSON export.

use std::fs;

use anyhow::{Context, Result};
use serde::Serialize;

use super::objective::BacktestMetrics;

/// A single completed optimization trial.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedTrial<T> {
    pub number: u32,
    pub params: T,
    pub metrics: BacktestMetrics,
    pub obj0: f64,
    pub obj1: f64,
}

/// Returns `true` when both objectives are usable (finite) for ranking.
fn has_finite_objectives<T>(r: &RecordedTrial<T>) -> bool {
    r.obj0.is_finite() && r.obj1.is_finite()
}

/// Returns the non-dominated (Pareto optimal) trials. Both objectives are minimized.
///
/// Trials with non-finite objectives (failed or degenerate backtests) are excluded, since an
/// `INF`/`NaN` objective would otherwise pollute the front.
pub fn pareto_front<T>(records: &[RecordedTrial<T>]) -> Vec<&RecordedTrial<T>> {
    let mut front: Vec<&RecordedTrial<T>> = Vec::new();
    for r in records {
        if !has_finite_objectives(r) {
            continue;
        }
        let dominated = records.iter().any(|other| {
            other.number != r.number
                && has_finite_objectives(other)
                && other.obj0 <= r.obj0
                && other.obj1 <= r.obj1
                && (other.obj0 < r.obj0 || other.obj1 < r.obj1)
        });
        if !dominated {
            front.push(r);
        }
    }
    front.sort_by(|a, b| {
        (a.obj0 + a.obj1)
            .partial_cmp(&(b.obj0 + b.obj1))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    front
}

/// Ranks the best `n` trials by a weighted scalar of the two objectives.
///
/// Trials with non-finite objectives are excluded.
pub fn top_n_scored<T>(records: &[RecordedTrial<T>], n: usize, weight_obj0: f64) -> Vec<&RecordedTrial<T>> {
    let mut scored: Vec<&RecordedTrial<T>> = records
        .iter()
        .filter(|r| has_finite_objectives(r))
        .collect();
    scored.sort_by(|a, b| {
        let sa = weight_obj0 * a.obj0 + (1.0 - weight_obj0) * a.obj1;
        let sb = weight_obj0 * b.obj0 + (1.0 - weight_obj0) * b.obj1;
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(n);
    scored
}

/// Persistent JSON serialization of a study.
pub fn write_study_json<T: Serialize>(
    path: &str,
    study_name: &str,
    sampler: &str,
    seed: Option<u64>,
    records: &[RecordedTrial<T>],
) -> Result<()> {
    #[derive(Serialize)]
    struct StudyJson<'a, U> {
        study_name: &'a str,
        sampler: &'a str,
        seed: Option<u64>,
        n_trials: usize,
        trials: &'a [RecordedTrial<U>],
    }
    let json = StudyJson {
        study_name,
        sampler,
        seed,
        n_trials: records.len(),
        trials: records,
    };
    let contents = serde_json::to_string_pretty(&json)?;
    fs::write(path, contents).with_context(|| format!("failed to write study to {path}"))
}

/// Prints a table of trial records. `describe` renders the strategy-specific parameter set.
pub fn print_trials<T>(title: &str, records: &[&RecordedTrial<T>], describe: impl Fn(&T) -> String) {
    println!("\n{title}");
    println!(
        "{:<6} {:>8} {:>8} {:>8} {:>9} {:>9} {:>7}",
        "trial", "sqn", "mdd%", "sharpe", "obj0", "obj1", "orders"
    );
    for r in records {
        println!(
            "{:<6} {:>8.2} {:>7.2}% {:>8.2} {:>8.2} {:>8.2} {:>8}",
            r.number,
            r.metrics.sqn,
            r.metrics.max_drawdown * 100.0,
            r.metrics.sharpe_252.unwrap_or(f64::NAN),
            r.obj0,
            r.obj1,
            r.metrics.total_orders,
        );
        println!("      {}", describe(&r.params));
    }
}
