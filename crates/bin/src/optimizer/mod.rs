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

//! Strategy-agnostic parameter optimization.

pub mod objective;
pub mod param;
pub mod report;
pub mod sampler;

pub use objective::{run_backtest, BacktestEnv, BacktestMetrics, SQN_DD_PENALTY};
pub use param::{ParamKind, ParamSpec, SearchSpace};
pub use report::{pareto_front, print_trials, top_n_scored, write_study_json, RecordedTrial};
pub use sampler::{Optimizable, RustunaOptimizer, SamplerKind};
