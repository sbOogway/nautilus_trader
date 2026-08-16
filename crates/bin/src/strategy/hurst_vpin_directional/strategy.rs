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

//! Hurst/VPIN directional strategy implementation.

use std::{collections::VecDeque, fmt::Debug, time::Duration};

use ahash::AHashSet;
use nautilus_common::{actor::DataActor, timer::TimeEvent};
use nautilus_model::{
    data::{Bar, TradeTick},
    enums::{AggressorSide, OrderSide, PositionSide, TimeInForce::Ioc},
    events::{
        OrderCanceled, OrderDenied, OrderExpired, OrderFilled, OrderRejected, PositionClosed,
        PositionOpened,
    },
    identifiers::{ClientOrderId, PositionId},
    orders::{Order, OrderCore},
    types::Quantity,
};
use nautilus_trading::{Strategy, StrategyCore, nautilus_strategy};

use crate::strategy::hurst_vpin_directional::config::HurstVpinDirectionalConfig;

/// Name of the timer driving entry and holding-time checks.
const TIMER_NAME: &str = "HURST_VPIN_TIMER";

/// Directional strategy combining a Hurst-exponent regime filter on dollar bars
/// with a VPIN (Volume-synchronized Probability of Informed Trading) signal
/// derived from trade aggressor flow, with entry timing gated on a timer.
///
/// The strategy is sampled on information-driven (value) bars rather than
/// clock time, following Lopez de Prado (*Advances in Financial Machine
/// Learning*, Chapter 2). The Hurst exponent is estimated by rescaled range
/// over the window of dollar bar log returns. VPIN is averaged over completed
/// volume buckets, with a signed variant carrying the net informed direction.
///
/// Unlike the reference implementation in `nautilus_trading` examples, entry
/// timing is gated on a timer rather than the live quote stream so the
/// strategy behaves identically in backtest and live.
pub struct HurstVpinDirectional {
    pub(super) core: StrategyCore,
    pub(super) config: HurstVpinDirectionalConfig,
    pub(super) returns: VecDeque<f64>,
    pub(super) abs_imbalances: VecDeque<f64>,
    pub(super) signed_imbalances: VecDeque<f64>,
    pub(super) last_close: Option<f64>,
    pub(super) bucket_buy_volume: f64,
    pub(super) bucket_sell_volume: f64,
    pub(super) hurst: Option<f64>,
    pub(super) vpin: Option<f64>,
    pub(super) signed_vpin: Option<f64>,
    pub(super) position_opened_ns: Option<u64>,
    pub(super) exit_cooldown: bool,
    pub(super) entry_order_id: Option<ClientOrderId>,
    pub(super) exit_order_ids: AHashSet<ClientOrderId>,
}

impl HurstVpinDirectional {
    /// Creates a new [`HurstVpinDirectional`] instance from config.
    #[must_use]
    pub fn new(config: HurstVpinDirectionalConfig) -> Self {
        let hurst_window = config.hurst_window;
        let vpin_window = config.vpin_window;
        Self {
            core: StrategyCore::new(config.base.clone()),
            config,
            returns: VecDeque::with_capacity(hurst_window),
            abs_imbalances: VecDeque::with_capacity(vpin_window),
            signed_imbalances: VecDeque::with_capacity(vpin_window),
            last_close: None,
            bucket_buy_volume: 0.0,
            bucket_sell_volume: 0.0,
            hurst: None,
            vpin: None,
            signed_vpin: None,
            position_opened_ns: None,
            exit_cooldown: false,
            entry_order_id: None,
            exit_order_ids: AHashSet::new(),
        }
    }

    pub(super) fn signals_ready(&self) -> bool {
        self.hurst.is_some()
            && self.vpin.is_some()
            && self.signed_vpin.is_some()
            && self.returns.len() == self.config.hurst_window
            && self.abs_imbalances.len() == self.config.vpin_window
    }

    pub(super) fn push_bounded(values: &mut VecDeque<f64>, capacity: usize, value: f64) {
        if values.len() == capacity {
            values.pop_front();
        }
        values.push_back(value);
    }

    pub(super) fn rolling_mean(values: &VecDeque<f64>) -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        Some(values.iter().copied().sum::<f64>() / values.len() as f64)
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "R/S regression is inherently nested"
    )]
    pub(super) fn estimate_hurst(&self) -> Option<f64> {
        if self.returns.len() < self.config.hurst_window {
            return None;
        }

        let returns: Vec<f64> = self.returns.iter().copied().collect();
        let mut log_lags: Vec<f64> = Vec::new();
        let mut log_rs: Vec<f64> = Vec::new();

        for &lag in &self.config.hurst_lags {
            if lag < 2 || lag > returns.len() {
                continue;
            }

            let mut rs_values: Vec<f64> = Vec::new();

            for start in (0..=returns.len().saturating_sub(lag)).step_by(lag) {
                let chunk = &returns[start..start + lag];
                let mean = chunk.iter().sum::<f64>() / lag as f64;

                let mut running = 0.0f64;
                let mut cum_min = 0.0f64;
                let mut cum_max = 0.0f64;
                let mut var_sum = 0.0f64;

                for value in chunk {
                    let deviation = value - mean;
                    running += deviation;
                    if running < cum_min {
                        cum_min = running;
                    }

                    if running > cum_max {
                        cum_max = running;
                    }
                    var_sum += deviation * deviation;
                }
                let r_range = cum_max - cum_min;
                let stdev = (var_sum / lag as f64).sqrt();
                if r_range > 0.0 && stdev > 0.0 {
                    rs_values.push(r_range / stdev);
                }
            }

            if !rs_values.is_empty() {
                let avg_rs = rs_values.iter().copied().sum::<f64>() / rs_values.len() as f64;
                log_lags.push((lag as f64).ln());
                log_rs.push(avg_rs.ln());
            }
        }

        if log_lags.len() < 2 {
            return None;
        }

        let n = log_lags.len() as f64;
        let sx: f64 = log_lags.iter().sum();
        let sy: f64 = log_rs.iter().sum();
        let sxx: f64 = log_lags.iter().map(|x| x * x).sum();
        let sxy: f64 = log_lags.iter().zip(log_rs.iter()).map(|(x, y)| x * y).sum();
        let denom = n * sxx - sx * sx;
        if denom == 0.0 {
            return None;
        }
        Some((n * sxy - sx * sy) / denom)
    }

    pub(super) fn try_open_position(&mut self) -> anyhow::Result<()> {
        let (hurst, vpin, signed_vpin) = match (self.hurst, self.vpin, self.signed_vpin) {
            (Some(h), Some(v), Some(s)) => (h, v, s),
            _ => return Ok(()),
        };

        if hurst < self.config.hurst_enter || vpin < self.config.vpin_threshold {
            return Ok(());
        }

        if signed_vpin > 0.0 {
            self.submit_entry(OrderSide::Buy)?;
        } else if signed_vpin < 0.0 {
            self.submit_entry(OrderSide::Sell)?;
        }

        Ok(())
    }

    pub(super) fn check_regime_exit(&mut self) -> anyhow::Result<()> {
        if !self.exit_order_ids.is_empty() {
            return Ok(());
        }
        let hurst = match self.hurst {
            Some(h) => h,
            None => return Ok(()),
        };

        if hurst >= self.config.hurst_exit {
            return Ok(());
        }

        let has_open_position = self.has_open_position();
        if !has_open_position {
            return Ok(());
        }

        log::info!("Hurst regime decay (Hurst={hurst:.3}); closing position");
        self.submit_close()
    }

    pub(super) fn check_holding_timeout(&mut self, now_ns: u64) -> anyhow::Result<()> {
        if !self.exit_order_ids.is_empty() {
            return Ok(());
        }
        let opened_ns = match self.position_opened_ns {
            Some(ns) => ns,
            None => return Ok(()),
        };
        let held_ns = now_ns.saturating_sub(opened_ns);
        if held_ns < self.config.max_holding_secs * 1_000_000_000 {
            return Ok(());
        }

        log::info!("Holding timeout reached; closing position");
        self.submit_close()
    }

    fn submit_entry(&mut self, side: OrderSide) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        let trade_size = self.config.trade_size;
        let order = self.order().market(
            instrument_id,
            side,
            trade_size,
            Some(Ioc),
            None, // reduce_only
            None, // quote_quantity
            None, // exec_algorithm_id
            None, // exec_algorithm_params
            None, // tags
            None, // client_order_id
        );
        self.entry_order_id = Some(order.client_order_id());
        self.submit_order(order, None, None, None)
    }

    fn submit_close(&mut self) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        let strategy_id = self.strategy_id().expect("Strategy must be registered");

        let positions: Vec<(PositionId, Quantity, PositionSide)> = self
            .cache()
            .positions_open(None, Some(&instrument_id), Some(&strategy_id), None, None)
            .iter()
            .map(|p| (p.id, p.quantity, p.side))
            .collect();

        if positions.is_empty() {
            return Ok(());
        }

        self.exit_cooldown = true;

        for (position_id, quantity, side) in positions {
            let closing_side = OrderCore::closing_side(side);
            let close_order = self.order().market(
                instrument_id,
                closing_side,
                quantity,
                Some(Ioc),
                Some(true), // reduce_only
                None,
                None,
                None,
                None,
                None,
            );
            self.exit_order_ids.insert(close_order.client_order_id());
            self.submit_order(close_order, Some(position_id), None, None)?;
        }

        Ok(())
    }

    fn has_open_position(&self) -> bool {
        let instrument_id = self.config.instrument_id;
        let strategy_id = self.strategy_id().expect("Strategy must be registered");
        !self
            .cache()
            .positions_open(None, Some(&instrument_id), Some(&strategy_id), None, None)
            .is_empty()
    }

    fn clear_latch_for(&mut self, client_order_id: ClientOrderId) {
        if self.entry_order_id == Some(client_order_id) {
            self.entry_order_id = None;
        }
        self.exit_order_ids.remove(&client_order_id);
    }
}

nautilus_strategy!(HurstVpinDirectional, {
    fn on_position_opened(&mut self, event: PositionOpened) {
        if event.instrument_id == self.config.instrument_id {
            self.position_opened_ns = Some(event.ts_event.as_u64());
        }
    }

    fn on_position_closed(&mut self, event: PositionClosed) {
        if event.instrument_id == self.config.instrument_id {
            self.position_opened_ns = None;
        }
    }

    fn on_order_rejected(&mut self, event: OrderRejected) {
        if event.instrument_id == self.config.instrument_id {
            self.clear_latch_for(event.client_order_id);
        }
    }

    fn on_order_expired(&mut self, event: OrderExpired) {
        if event.instrument_id == self.config.instrument_id {
            self.clear_latch_for(event.client_order_id);
        }
    }

    fn on_order_denied(&mut self, event: OrderDenied) {
        if event.instrument_id == self.config.instrument_id {
            self.clear_latch_for(event.client_order_id);
        }
    }

    fn on_order_filled(&mut self, event: &OrderFilled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }

        let closed = self
            .cache()
            .order(&event.client_order_id)
            .is_some_and(|order| order.is_closed());

        if closed {
            self.clear_latch_for(event.client_order_id);
        }
    }

    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        if event.instrument_id != self.config.instrument_id {
            return;
        }
        self.clear_latch_for(event.client_order_id);
    }
});

impl Debug for HurstVpinDirectional {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(HurstVpinDirectional))
            .field("config", &self.config)
            .field("hurst", &self.hurst)
            .field("vpin", &self.vpin)
            .field("signed_vpin", &self.signed_vpin)
            .finish()
    }
}

impl DataActor for HurstVpinDirectional {
    fn on_start(&mut self) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        let bar_instrument_id = self.config.bar_type.instrument_id();
        if bar_instrument_id != instrument_id {
            anyhow::bail!(
                "bar_type instrument {bar_instrument_id} does not match traded instrument {instrument_id}"
            );
        }
        {
            let cache = self.cache();
            cache.try_instrument(&instrument_id)?;
        }

        self.subscribe_bars(self.config.bar_type, None, None);
        self.subscribe_trades(instrument_id, None, None);

        self.clock().set_timer(
            TIMER_NAME,
            Duration::from_millis(self.config.timer_interval_ms),
            None,
            None,
            None,
            None,
            None,
        )?;

        log::info!(
            "Hurst/VPIN started: instrument={instrument_id}, bar_type={}, trade_size={}, \
             hurst_window={}, hurst_lags={:?}, hurst_enter={}, hurst_exit={}, vpin_window={}, \
             vpin_threshold={}, max_holding_secs={}, timer_interval_ms={}",
            self.config.bar_type,
            self.config.trade_size,
            self.config.hurst_window,
            self.config.hurst_lags,
            self.config.hurst_enter,
            self.config.hurst_exit,
            self.config.vpin_window,
            self.config.vpin_threshold,
            self.config.max_holding_secs,
            self.config.timer_interval_ms,
        );

        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.clock().cancel_timer(TIMER_NAME);
        let instrument_id = self.config.instrument_id;
        self.cancel_all_orders(instrument_id, None, None, None)?;
        self.close_all_positions(instrument_id, None, None, None, None, None, None, None)?;
        self.unsubscribe_bars(self.config.bar_type, None, None);
        self.unsubscribe_trades(instrument_id, None, None);
        Ok(())
    }

    fn on_time_event(&mut self, event: &TimeEvent) -> anyhow::Result<()> {
        if event.name != TIMER_NAME {
            return Ok(());
        }

        let now_ns = event.ts_event.as_u64();

        if !self.signals_ready() {
            return Ok(());
        }

        if self.has_open_position() {
            return self.check_holding_timeout(now_ns);
        }

        if self.exit_cooldown {
            return Ok(());
        }

        if self.entry_order_id.is_some() || !self.exit_order_ids.is_empty() {
            return Ok(());
        }

        let strategy_id = self.strategy_id().expect("Strategy must be registered");
        let has_working = {
            let cache = self.cache();
            !cache
                .orders_open(
                    None,
                    Some(&self.config.instrument_id),
                    Some(&strategy_id),
                    None,
                    None,
                )
                .is_empty()
                || !cache
                    .orders_inflight(
                        None,
                        Some(&self.config.instrument_id),
                        Some(&strategy_id),
                        None,
                        None,
                    )
                    .is_empty()
        };

        if has_working {
            return Ok(());
        }

        self.try_open_position()
    }

    fn on_trade(&mut self, tick: &TradeTick) -> anyhow::Result<()> {
        let size = tick.size.as_f64();
        match tick.aggressor_side {
            AggressorSide::Buyer => self.bucket_buy_volume += size,
            AggressorSide::Seller => self.bucket_sell_volume += size,
            _ => {}
        }
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let close = bar.close.as_f64();

        if let Some(prev) = self.last_close
            && prev > 0.0
            && close > 0.0
        {
            let window = self.config.hurst_window;
            Self::push_bounded(&mut self.returns, window, (close / prev).ln());
        }
        self.last_close = Some(close);

        let total = self.bucket_buy_volume + self.bucket_sell_volume;
        if total > 0.0 {
            let imbalance = (self.bucket_buy_volume - self.bucket_sell_volume) / total;
            let vpin_window = self.config.vpin_window;
            Self::push_bounded(&mut self.abs_imbalances, vpin_window, imbalance.abs());
            Self::push_bounded(&mut self.signed_imbalances, vpin_window, imbalance);
        }
        self.bucket_buy_volume = 0.0;
        self.bucket_sell_volume = 0.0;

        self.hurst = self.estimate_hurst();
        self.vpin = Self::rolling_mean(&self.abs_imbalances);
        self.signed_vpin = Self::rolling_mean(&self.signed_imbalances);

        if let Some(h) = self.hurst {
            log::info!(
                "Hurst={h:.3} VPIN={:.3} signed={:+.3} bar_close={close:.2}",
                self.vpin.unwrap_or(0.0),
                self.signed_vpin.unwrap_or(0.0),
            );
        }

        self.exit_cooldown = false;
        self.check_regime_exit()
    }

    fn on_reset(&mut self) -> anyhow::Result<()> {
        self.returns.clear();
        self.abs_imbalances.clear();
        self.signed_imbalances.clear();
        self.last_close = None;
        self.bucket_buy_volume = 0.0;
        self.bucket_sell_volume = 0.0;
        self.hurst = None;
        self.vpin = None;
        self.signed_vpin = None;
        self.position_opened_ns = None;
        self.exit_cooldown = false;
        self.entry_order_id = None;
        self.exit_order_ids.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use nautilus_common::{
        actor::DataActor,
        cache::Cache,
        clock::{Clock, TestClock},
    };
    use nautilus_model::{
        data::{Bar, BarSpecification, BarType, TradeTick},
        enums::{AggregationSource, AggressorSide, BarAggregation, PriceType},
        identifiers::{ClientOrderId, InstrumentId, StrategyId, Symbol, TradeId, TraderId},
        instruments::{CryptoPerpetual, InstrumentAny},
        types::{Currency, Price, Quantity},
    };
    use nautilus_portfolio::portfolio::Portfolio;
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::{HurstVpinDirectional, HurstVpinDirectionalConfig};

    fn pf_xbtusd() -> CryptoPerpetual {
        CryptoPerpetual::new(
            InstrumentId::from("PF_XBTUSD.KRAKEN"),
            Symbol::from("PF_XBTUSD"),
            Currency::BTC(),
            Currency::USD(),
            Currency::USD(),
            false,
            1,
            4,
            Price::from("0.5"),
            Quantity::from("0.0001"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(dec!(0.02)),
            Some(dec!(0.01)),
            Some(dec!(0.0002)),
            Some(dec!(0.0005)),
            None,
            None,
            0.into(),
            0.into(),
        )
    }

    fn bar_type(instrument_id: InstrumentId) -> BarType {
        BarType::new(
            instrument_id,
            BarSpecification::new(2_000_000, BarAggregation::Value, PriceType::Last),
            AggregationSource::Internal,
        )
    }

    fn create_strategy(instrument_id: InstrumentId) -> HurstVpinDirectional {
        let config = HurstVpinDirectionalConfig::builder()
            .instrument_id(instrument_id)
            .bar_type(bar_type(instrument_id))
            .trade_size(Quantity::from("0.01"))
            .build();
        HurstVpinDirectional::new(config)
    }

    fn create_strategy_with_windows(
        instrument_id: InstrumentId,
        hurst_window: usize,
        hurst_lags: Vec<usize>,
    ) -> HurstVpinDirectional {
        let config = HurstVpinDirectionalConfig::builder()
            .instrument_id(instrument_id)
            .bar_type(bar_type(instrument_id))
            .trade_size(Quantity::from("0.01"))
            .hurst_window(hurst_window)
            .hurst_lags(hurst_lags)
            .build();
        HurstVpinDirectional::new(config)
    }

    fn register_strategy(strategy: &mut HurstVpinDirectional) {
        let trader_id = TraderId::from("TESTER-001");
        let clock: Rc<RefCell<dyn Clock>> = Rc::new(RefCell::new(TestClock::new()));
        let cache = Rc::new(RefCell::new(Cache::default()));
        let portfolio = Rc::new(RefCell::new(Portfolio::new(
            clock.clone(),
            cache.clone(),
            None,
        )));
        strategy
            .core
            .register(trader_id, clock, cache, portfolio)
            .unwrap();
    }

    fn trade(instrument_id: InstrumentId, size: &str, side: AggressorSide, ts: u64) -> TradeTick {
        TradeTick::new(
            instrument_id,
            Price::from("30000.0"),
            Quantity::from(size),
            side,
            TradeId::from("T-1"),
            ts.into(),
            ts.into(),
        )
    }

    fn bar(bar_type: BarType, close: &str, ts: u64) -> Bar {
        Bar::new(
            bar_type,
            Price::from(close),
            Price::from(close),
            Price::from(close),
            Price::from(close),
            Quantity::from("1"),
            ts.into(),
            ts.into(),
        )
    }

    #[rstest]
    fn test_new_initializes_clean_state() {
        let strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));

        assert!(strategy.hurst.is_none());
        assert!(strategy.vpin.is_none());
        assert!(strategy.signed_vpin.is_none());
        assert!(strategy.last_close.is_none());
        assert_eq!(strategy.bucket_buy_volume, 0.0);
        assert_eq!(strategy.bucket_sell_volume, 0.0);
        assert!(!strategy.exit_cooldown);
        assert!(strategy.entry_order_id.is_none());
        assert!(strategy.exit_order_ids.is_empty());
        assert!(strategy.position_opened_ns.is_none());
    }

    #[rstest]
    fn test_config_defaults() {
        let instrument_id = InstrumentId::from("PF_XBTUSD.KRAKEN");
        let strategy = create_strategy(instrument_id);
        let config = &strategy.config;

        assert_eq!(config.hurst_window, 128);
        assert_eq!(config.hurst_lags, vec![4, 8, 16, 32]);
        assert_eq!(config.hurst_enter, 0.55);
        assert_eq!(config.hurst_exit, 0.50);
        assert_eq!(config.vpin_window, 50);
        assert_eq!(config.vpin_threshold, 0.30);
        assert_eq!(config.max_holding_secs, 3600);
        assert_eq!(config.timer_interval_ms, 1000);
        assert_eq!(
            strategy.core.config.strategy_id,
            Some(StrategyId::from("HURST_VPIN-001")),
        );
    }

    #[rstest]
    fn test_buyer_aggressor_adds_to_buy_volume() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));

        strategy
            .on_trade(&trade(
                strategy.config.instrument_id,
                "5.0",
                AggressorSide::Buyer,
                1,
            ))
            .unwrap();

        assert_eq!(strategy.bucket_buy_volume, 5.0);
        assert_eq!(strategy.bucket_sell_volume, 0.0);
    }

    #[rstest]
    fn test_seller_aggressor_adds_to_sell_volume() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));

        strategy
            .on_trade(&trade(
                strategy.config.instrument_id,
                "7.0",
                AggressorSide::Seller,
                1,
            ))
            .unwrap();

        assert_eq!(strategy.bucket_buy_volume, 0.0);
        assert_eq!(strategy.bucket_sell_volume, 7.0);
    }

    #[rstest]
    fn test_no_aggressor_is_ignored() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));

        strategy
            .on_trade(&trade(
                strategy.config.instrument_id,
                "3.0",
                AggressorSide::NoAggressor,
                1,
            ))
            .unwrap();

        assert_eq!(strategy.bucket_buy_volume, 0.0);
        assert_eq!(strategy.bucket_sell_volume, 0.0);
    }

    #[rstest]
    fn test_first_bar_records_close_but_not_return() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        register_strategy(&mut strategy);
        let cache = Rc::new(RefCell::new(Cache::default()));
        cache
            .borrow_mut()
            .add_instrument(InstrumentAny::CryptoPerpetual(pf_xbtusd()))
            .unwrap();

        let bt = bar_type(strategy.config.instrument_id);
        strategy.on_bar(&bar(bt, "30000.0", 0)).unwrap();

        assert_eq!(strategy.last_close, Some(30000.0));
        assert_eq!(strategy.returns.len(), 0);
    }

    #[rstest]
    fn test_second_bar_appends_log_return() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        register_strategy(&mut strategy);

        let bt = bar_type(strategy.config.instrument_id);
        strategy.on_bar(&bar(bt, "30000.0", 0)).unwrap();
        strategy.on_bar(&bar(bt, "33000.0", 1)).unwrap();

        assert_eq!(strategy.returns.len(), 1);
        let expected = (33000.0_f64 / 30000.0_f64).ln();
        assert!((strategy.returns[0] - expected).abs() < 1e-9);
    }

    #[rstest]
    fn test_zero_volume_bar_does_not_record_imbalance() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        register_strategy(&mut strategy);

        let bt = bar_type(strategy.config.instrument_id);
        strategy.on_bar(&bar(bt, "30000.0", 0)).unwrap();
        strategy.on_bar(&bar(bt, "30010.0", 1)).unwrap();

        assert!(strategy.abs_imbalances.is_empty());
        assert!(strategy.signed_imbalances.is_empty());
    }

    #[rstest]
    fn test_bar_finalizes_bucket_and_resets_accumulators() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        register_strategy(&mut strategy);

        let bt = bar_type(strategy.config.instrument_id);
        strategy.on_bar(&bar(bt, "30000.0", 0)).unwrap();
        strategy
            .on_trade(&trade(
                strategy.config.instrument_id,
                "7.0",
                AggressorSide::Buyer,
                1,
            ))
            .unwrap();
        strategy
            .on_trade(&trade(
                strategy.config.instrument_id,
                "3.0",
                AggressorSide::Seller,
                2,
            ))
            .unwrap();
        strategy.on_bar(&bar(bt, "30010.0", 3)).unwrap();

        assert_eq!(strategy.abs_imbalances.len(), 1);
        assert!((strategy.abs_imbalances[0] - 0.4).abs() < 1e-9);
        assert_eq!(strategy.signed_imbalances.len(), 1);
        assert!((strategy.signed_imbalances[0] - 0.4).abs() < 1e-9);
        assert_eq!(strategy.bucket_buy_volume, 0.0);
        assert_eq!(strategy.bucket_sell_volume, 0.0);
    }

    #[rstest]
    fn test_bar_clears_exit_cooldown() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        register_strategy(&mut strategy);
        strategy.exit_cooldown = true;

        let bt = bar_type(strategy.config.instrument_id);
        strategy.on_bar(&bar(bt, "30000.0", 0)).unwrap();

        assert!(!strategy.exit_cooldown);
    }

    #[rstest]
    fn test_hurst_returns_none_when_insufficient_returns() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        // Fewer than hurst_window (128)
        for value in [0.01, -0.01, 0.01] {
            strategy.returns.push_back(value);
        }
        assert!(strategy.estimate_hurst().is_none());
    }

    #[rstest]
    fn test_hurst_mean_reverting_series_below_half() {
        let mut strategy = create_strategy_with_windows(
            InstrumentId::from("PF_XBTUSD.KRAKEN"),
            128,
            vec![4, 8, 16, 32],
        );

        for i in 0..strategy.config.hurst_window {
            strategy
                .returns
                .push_back(if i % 2 == 0 { 0.01 } else { -0.01 });
        }

        let h = strategy.estimate_hurst().unwrap();
        assert!(h < 0.30, "expected mean-reverting Hurst < 0.30, was {h}");
    }

    #[rstest]
    fn test_hurst_persistent_series_above_half() {
        // AR(1) with positive coefficient produces positively autocorrelated returns
        let mut strategy = create_strategy_with_windows(
            InstrumentId::from("PF_XBTUSD.KRAKEN"),
            128,
            vec![4, 8, 16, 32],
        );
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut prev = 0.0_f64;

        for _ in 0..strategy.config.hurst_window {
            // Seeded xorshift for deterministic noise
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let u = (state as f64 / u64::MAX as f64 - 0.5) * 0.002;
            let value = 0.9 * prev + u;
            strategy.returns.push_back(value);
            prev = value;
        }

        let h = strategy.estimate_hurst().unwrap();
        assert!(h > 0.70, "expected persistent Hurst > 0.70, was {h}");
    }

    #[rstest]
    fn test_rolling_mean_on_empty_returns_none() {
        let empty: VecDeque<f64> = VecDeque::new();
        assert!(HurstVpinDirectional::rolling_mean(&empty).is_none());
    }

    #[rstest]
    fn test_rolling_mean_basic_average() {
        let mut values: VecDeque<f64> = VecDeque::new();
        values.push_back(1.0);
        values.push_back(2.0);
        values.push_back(3.0);
        assert_eq!(HurstVpinDirectional::rolling_mean(&values), Some(2.0));
    }

    #[rstest]
    fn test_push_bounded_respects_capacity() {
        let mut values: VecDeque<f64> = VecDeque::new();
        HurstVpinDirectional::push_bounded(&mut values, 3, 1.0);
        HurstVpinDirectional::push_bounded(&mut values, 3, 2.0);
        HurstVpinDirectional::push_bounded(&mut values, 3, 3.0);
        HurstVpinDirectional::push_bounded(&mut values, 3, 4.0);

        assert_eq!(values.len(), 3);
        assert_eq!(values[0], 2.0);
        assert_eq!(values[2], 4.0);
    }

    #[rstest]
    fn test_signals_ready_false_during_warmup() {
        let strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        assert!(!strategy.signals_ready());
    }

    #[rstest]
    fn test_signals_ready_true_when_windows_filled() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        for _ in 0..strategy.config.hurst_window {
            strategy.returns.push_back(0.001);
        }
        for _ in 0..strategy.config.vpin_window {
            strategy.abs_imbalances.push_back(0.3);
            strategy.signed_imbalances.push_back(0.2);
        }
        strategy.hurst = Some(0.6);
        strategy.vpin = Some(0.3);
        strategy.signed_vpin = Some(0.2);

        assert!(strategy.signals_ready());
    }

    #[rstest]
    fn test_on_start_rejects_mismatched_bar_type() {
        let instrument_id = InstrumentId::from("PF_XBTUSD.KRAKEN");
        let other_id = InstrumentId::from("OTHER.KRAKEN");
        let config = HurstVpinDirectionalConfig::builder()
            .instrument_id(instrument_id)
            .bar_type(bar_type(other_id))
            .trade_size(Quantity::from("0.01"))
            .build();
        let mut strategy = HurstVpinDirectional::new(config);
        register_strategy(&mut strategy);

        let err = strategy.on_start().unwrap_err();
        assert!(
            err.to_string().contains("does not match traded instrument"),
            "unexpected error: {err}"
        );
    }

    #[rstest]
    fn test_on_reset_clears_all_state() {
        let mut strategy = create_strategy(InstrumentId::from("PF_XBTUSD.KRAKEN"));
        register_strategy(&mut strategy);

        strategy.returns.push_back(0.01);
        strategy.abs_imbalances.push_back(0.4);
        strategy.signed_imbalances.push_back(-0.3);
        strategy.last_close = Some(30000.0);
        strategy.bucket_buy_volume = 1.0;
        strategy.bucket_sell_volume = 2.0;
        strategy.hurst = Some(0.6);
        strategy.vpin = Some(0.4);
        strategy.signed_vpin = Some(0.4);
        strategy.position_opened_ns = Some(12_345);
        strategy.exit_cooldown = true;
        strategy.entry_order_id = Some(ClientOrderId::from("O-1"));
        strategy.exit_order_ids.insert(ClientOrderId::from("O-2"));

        strategy.on_reset().unwrap();

        assert!(strategy.returns.is_empty());
        assert!(strategy.abs_imbalances.is_empty());
        assert!(strategy.signed_imbalances.is_empty());
        assert!(strategy.last_close.is_none());
        assert_eq!(strategy.bucket_buy_volume, 0.0);
        assert_eq!(strategy.bucket_sell_volume, 0.0);
        assert!(strategy.hurst.is_none());
        assert!(strategy.vpin.is_none());
        assert!(strategy.signed_vpin.is_none());
        assert!(strategy.position_opened_ns.is_none());
        assert!(!strategy.exit_cooldown);
        assert!(strategy.entry_order_id.is_none());
        assert!(strategy.exit_order_ids.is_empty());
    }
}
