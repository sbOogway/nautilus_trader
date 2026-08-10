//! Configuration for the Avellaneda-Stoikov market making strategy.

use nautilus_model::{
    identifiers::{InstrumentId, StrategyId},
    types::Quantity,
};
use nautilus_trading::StrategyConfig;

#[allow(non_snake_case)]
#[derive(Debug, Clone, bon::Builder)]
pub struct AvellanedaStoikovConfig {
    #[builder(default = StrategyConfig {
        strategy_id: Some(StrategyId::from("AS-001")),
        order_id_tag: Some("004".to_string()),
        ..Default::default()
    })]
    pub base: StrategyConfig,
    pub instrument_id: InstrumentId,
    pub trade_size: Option<Quantity>,
    pub gamma: f64,
    pub sigma: f64,
    pub kappa: f64,
    pub arrival_rate: f64,
    pub time_horizon_secs: f64,
    pub lookback_secs: u64,
    pub expire_time_secs: Option<u64>,
}
