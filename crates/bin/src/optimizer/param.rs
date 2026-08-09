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

//! Search-space parameter definitions, driven by the `[optimize.params]` config section.

use anyhow::{bail, Result};
use rustuna_core::trial::Trial;
use serde::{Deserialize, Serialize};

/// A single sampled parameter kind and its inclusive range.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ParamKind {
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64 },
}

impl ParamKind {
    /// Validates the range invariants (inclusive bounds, `min <= max`).
    pub fn validate(&self, name: &str) -> Result<()> {
        match *self {
            Self::Int { min, max } => {
                if min > max {
                    bail!("param `{name}`: int min {min} exceeds max {max}");
                }
            }
            Self::Float { min, max } => {
                if !min.is_finite() || !max.is_finite() {
                    bail!("param `{name}`: float bounds must be finite");
                }
                if min > max {
                    bail!("param `{name}`: float min {min} exceeds max {max}");
                }
            }
        }
        Ok(())
    }
}

/// A named parameter in the search space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    pub kind: ParamKind,
}

/// The full search space of an optimization run.
#[derive(Debug, Clone, Default)]
pub struct SearchSpace {
    pub params: Vec<ParamSpec>,
}

impl SearchSpace {
    pub fn get(&self, name: &str) -> Option<&ParamSpec> {
        self.params.iter().find(|p| p.name == name)
    }

    pub fn validate(&self, known: &[&'static str]) -> Result<()> {
        for spec in &self.params {
            if !known.contains(&spec.name.as_str()) {
                bail!(
                    "unknown param `{}` in search space (known: {})",
                    spec.name,
                    known.join(", ")
                );
            }
            spec.kind.validate(&spec.name)?;
        }
        Ok(())
    }
}

/// Samples an integer parameter from the search space, requiring it to be declared as `int`.
pub fn suggest_int(trial: &mut Trial, space: &SearchSpace, name: &'static str) -> Result<i64> {
    let spec = space
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("missing param `{name}` in [optimize.params]"))?;
    match spec.kind {
        ParamKind::Int { min, max } => Ok(
            trial
                .suggest_int(name, min, max)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        ParamKind::Float { .. } => bail!("param `{name}` must be declared as `type = \"int\"`"),
    }
}

/// Samples a float parameter from the search space, requiring it to be declared as `float`.
pub fn suggest_float(trial: &mut Trial, space: &SearchSpace, name: &'static str) -> Result<f64> {
    let spec = space
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("missing param `{name}` in [optimize.params]"))?;
    match spec.kind {
        ParamKind::Float { min, max } => Ok(
            trial
                .suggest_float(name, min, max)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        ParamKind::Int { .. } => bail!("param `{name}` must be declared as `type = \"float\"`"),
    }
}
