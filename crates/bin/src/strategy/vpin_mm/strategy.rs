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

//! VPIN market making strategy implementation.

use std::{collections::VecDeque, fmt::Debug, num::NonZeroUsize, time::Duration};

use ahash::AHashSet;
use nautilus_common::actor::DataActor;
use nautilus_model::{
    data::{OrderBookDeltas, TradeTick},
    enums::{BookType, OrderSide, TimeInForce},
    events::{OrderCanceled, OrderExpired, OrderFilled, OrderRejected},
    identifiers::ClientOrderId,
    instruments::{Instrument, InstrumentAny},
    orders::Order,
    types::{Price, Quantity},
};
use nautilus_trading::{Strategy, StrategyCore, nautilus_strategy};
use rust_decimal::Decimal;

use crate::strategy::vpin_mm::config::VpinMarketMakerConfig;

/// VPIN-based market making strategy with dynamic spread adjustment.
///
/// Places a symmetric grid of limit buy and sell orders around the mid-price,
/// similar to GridMarketMaker, but dynamically adjusts spacing based on
/// order flow toxicity measured by VPIN (Volume-Synchronized Probability
/// of Informed Trading).
///
/// When VPIN is high (toxic flow), the strategy widens spreads and increases
/// inventory skew to reduce adverse selection. When VPIN is extremely high,
/// all quotes are pulled to avoid being picked off by informed traders.
///
/// VPIN is computed over volume-synchronized buckets: trades are accumulated
/// until a target volume is reached, then the buy/sell imbalance is recorded
/// and VPIN is the rolling average of absolute imbalances.
///
/// Orders are placed both on timer events and on book delta updates (whichever
/// comes first), ensuring the strategy is responsive in both live and backtest
/// environments.
pub struct VpinMarketMaker {
    pub(super) core: StrategyCore,
    pub(super) config: VpinMarketMakerConfig,
    pub(super) instrument: Option<InstrumentAny>,
    pub(super) trade_size: Option<Quantity>,
    pub(super) price_precision: Option<u8>,
    pub(super) last_quoted_mid: Option<Price>,
    pub(super) pending_self_cancels: AHashSet<ClientOrderId>,
    pub(super) bucket_buy_volume: f64,
    pub(super) bucket_sell_volume: f64,
    pub(super) abs_imbalances: VecDeque<f64>,
    pub(super) signed_imbalances: VecDeque<f64>,
    pub(super) vpin: Option<f64>,
    pub(super) signed_vpin: Option<f64>,
}

impl VpinMarketMaker {
    /// Creates a new [`VpinMarketMaker`] instance from config.
    #[must_use]
    pub fn new(config: VpinMarketMakerConfig) -> Self {
        let vpin_window = config.vpin_window;
        Self {
            core: StrategyCore::new(config.base.clone()),
            instrument: None,
            trade_size: config.trade_size,
            config,
            price_precision: None,
            last_quoted_mid: None,
            pending_self_cancels: AHashSet::new(),
            bucket_buy_volume: 0.0,
            bucket_sell_volume: 0.0,
            abs_imbalances: VecDeque::with_capacity(vpin_window),
            signed_imbalances: VecDeque::with_capacity(vpin_window),
            vpin: None,
            signed_vpin: None,
        }
    }

    pub(super) fn should_requote(&self, mid: Price) -> bool {
        match self.last_quoted_mid {
            Some(last_mid) => {
                let last_f64 = last_mid.as_f64();
                if last_f64 == 0.0 {
                    return true;
                }
                let threshold = self.config.requote_threshold_bps as f64 / 10_000.0;
                (mid.as_f64() - last_f64).abs() / last_f64 >= threshold
            }
            None => true,
        }
    }

    /// Computes the dynamic grid orders with VPIN-adjusted spread and skew.
    ///
    /// When VPIN is available, the effective grid spacing is widened:
    /// `effective_bps = grid_step_bps * (1 + alpha * VPIN)`
    ///
    /// The inventory skew is also scaled up during toxic flow:
    /// `skew = skew_factor * net_position * (1 + VPIN)`
    pub(super) fn dynamic_grid_orders(
        &self,
        mid: Price,
        net_position: f64,
        worst_long: Decimal,
        worst_short: Decimal,
    ) -> anyhow::Result<Vec<(OrderSide, Price)>> {
        let Some(instrument) = self.instrument.as_ref() else {
            anyhow::bail!("Cannot compute grid orders: instrument is not resolved");
        };
        let mid_f64 = mid.as_f64();
        let vpin = self.vpin.unwrap_or(0.0);

        let vpin_multiplier = 1.0 + self.config.vpin_spread_alpha * vpin;
        let base_pct = self.config.grid_step_bps as f64 / 10_000.0;
        let effective_pct = base_pct * vpin_multiplier;

        let skew_f64 = self.config.skew_factor * net_position * (1.0 + vpin);

        let Some(trade_size) = self.trade_size else {
            anyhow::bail!("Cannot compute grid orders: trade_size is not resolved");
        };
        let trade_size = trade_size.as_decimal();
        let max_pos = self.config.max_position.as_decimal();
        let mut projected_long = worst_long;
        let mut projected_short = worst_short;
        let mut orders = Vec::new();

        for level in 1..=self.config.num_levels {
            let buy_f64 = mid_f64 * (1.0 - effective_pct).powi(level as i32) - skew_f64;
            let sell_f64 = mid_f64 * (1.0 + effective_pct).powi(level as i32) - skew_f64;
            let buy_price = instrument.next_bid_price(buy_f64, 0);
            let sell_price = instrument.next_ask_price(sell_f64, 0);

            if let Some(buy_price) = buy_price
                && projected_long + trade_size <= max_pos
            {
                orders.push((OrderSide::Buy, buy_price));
                projected_long += trade_size;
            }

            if let Some(sell_price) = sell_price
                && projected_short - trade_size >= -max_pos
            {
                orders.push((OrderSide::Sell, sell_price));
                projected_short -= trade_size;
            }
        }

        Ok(orders)
    }

    /// Checks if the current volume bucket is complete and, if so,
    /// computes the new VPIN value.
    fn maybe_complete_bucket(&mut self) {
        let total = self.bucket_buy_volume + self.bucket_sell_volume;
        if total >= self.config.bucket_target_volume {
            let imbalance = (self.bucket_buy_volume - self.bucket_sell_volume) / total;
            Self::push_bounded(
                &mut self.abs_imbalances,
                self.config.vpin_window,
                imbalance.abs(),
            );
            Self::push_bounded(
                &mut self.signed_imbalances,
                self.config.vpin_window,
                imbalance,
            );
            self.bucket_buy_volume = 0.0;
            self.bucket_sell_volume = 0.0;

            self.vpin = Self::rolling_mean(&self.abs_imbalances);
            self.signed_vpin = Self::rolling_mean(&self.signed_imbalances);

            if let Some(v) = self.vpin {
                log::info!(
                    "VPIN updated: {:.3} (signed: {:+.3}), bucket_volume={:.0}",
                    v,
                    self.signed_vpin.unwrap_or(0.0),
                    total,
                );
            }
        }
    }

    /// Computes worst-case per-side exposure including open positions and pending orders.
    fn compute_position_exposure(
        &self,
    ) -> anyhow::Result<(f64, Decimal, Decimal)> {
        let instrument_id = self.config.instrument_id;
        let strategy_id = self.strategy_id().expect("Strategy must be registered");
        let cache = self.cache();

        let mut position_qty = 0.0_f64;
        let mut position_dec = Decimal::ZERO;

        for p in cache.positions_open(None, Some(&instrument_id), Some(&strategy_id), None, None) {
            position_qty += p.signed_qty;
            position_dec += p.quantity.as_decimal()
                * if p.signed_qty < 0.0 {
                    Decimal::NEGATIVE_ONE
                } else {
                    Decimal::ONE
                };
        }

        let mut pending_buy_dec = Decimal::ZERO;
        let mut pending_sell_dec = Decimal::ZERO;
        let mut seen = AHashSet::new();

        let open = cache.orders_open(None, Some(&instrument_id), Some(&strategy_id), None, None);
        let inflight =
            cache.orders_inflight(None, Some(&instrument_id), Some(&strategy_id), None, None);
        for order in open.iter().chain(inflight.iter()) {
            if !seen.insert(order.client_order_id()) {
                continue;
            }
            let qty = order.leaves_qty().as_decimal();
            match order.order_side() {
                OrderSide::Buy => pending_buy_dec += qty,
                _ => pending_sell_dec += qty,
            }
        }

        Ok((
            position_qty,
            position_dec + pending_buy_dec,
            position_dec - pending_sell_dec,
        ))
    }

    /// Core order placement logic, shared between timer and book delta handlers.
    fn place_grid_orders(&mut self) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        let strategy_id = self.strategy_id().expect("Strategy must be registered");

        // Check if a new VPIN bucket can be completed
        self.maybe_complete_bucket();

        // If VPIN indicates extreme toxicity, pull all quotes
        if let Some(vpin) = self.vpin
            && vpin > self.config.vpin_pull_threshold
        {
            log::warn!(
                "VPIN toxicity too high ({:.3} > {:.3}); pulling quotes",
                vpin,
                self.config.vpin_pull_threshold,
            );
            self.cancel_all_orders(instrument_id, None, None, None)?;
            return Ok(());
        }

        let order_book = self
            .cache()
            .order_book(&instrument_id)
            .ok_or_else(|| {
                anyhow::anyhow!("Cannot requote grid: order book not found for {instrument_id}")
            })?;
        let (Some(bid_price), Some(ask_price)) =
            (order_book.best_bid_price(), order_book.best_ask_price())
        else {
            return Ok(());
        };

        let price_precision = self
            .price_precision
            .ok_or_else(|| anyhow::anyhow!("Cannot requote grid: price_precision is not resolved"))?;

        let mid_f64 = f64::midpoint(bid_price.as_f64(), ask_price.as_f64());
        let mid = Price::new(mid_f64, price_precision);

        // Check if we need to requote
        let has_resting = {
            let cache = self.cache();
            let venue = Some(&instrument_id.venue);
            let inst = Some(&instrument_id);
            let sid = Some(&strategy_id);
            cache.orders_open_count(venue, inst, sid, None, None) > 0
                || cache.orders_inflight_count(venue, inst, sid, None, None) > 0
        };

        if !self.should_requote(mid) && has_resting {
            return Ok(());
        }

        if let Some(vpin) = self.vpin {
            log::info!(
                "Requoting grid: mid={mid}, VPIN={:.3}, effective_bps={:.1}",
                vpin,
                self.config.grid_step_bps as f64 * (1.0 + self.config.vpin_spread_alpha * vpin),
            );
        } else {
            log::info!("Requoting grid: mid={mid}, VPIN=initializing");
        }

        // Cancel existing orders before placing new ones
        self.cancel_all_orders(instrument_id, None, None, None)?;

        // Compute position exposure
        let (net_position, worst_long, worst_short) = self.compute_position_exposure()?;

        // Compute dynamic grid orders
        let grid = self.dynamic_grid_orders(mid, net_position, worst_long, worst_short)?;

        if grid.is_empty() {
            return Ok(());
        }

        let trade_size = self
            .trade_size
            .ok_or_else(|| anyhow::anyhow!("Cannot requote grid: trade_size is not resolved"))?;

        let (tif, expire_time) = match self.config.expire_time_secs {
            Some(secs) => {
                let now_ns = self.clock().timestamp_ns();
                let expire_ns = now_ns + secs * 1_000_000_000;
                (Some(TimeInForce::Gtd), Some(expire_ns))
            }
            None => (None, None),
        };

        for (side, price) in grid {
            let order = self.order().limit(
                instrument_id,
                side,
                trade_size,
                price,
                tif,
                expire_time,
                Some(true), // post_only
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            );
            self.submit_order(order, None, None, None)?;
        }

        self.last_quoted_mid = Some(mid);
        Ok(())
    }

    fn push_bounded(values: &mut VecDeque<f64>, capacity: usize, value: f64) {
        if values.len() == capacity {
            values.pop_front();
        }
        values.push_back(value);
    }

    fn rolling_mean(values: &VecDeque<f64>) -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        Some(values.iter().copied().sum::<f64>() / values.len() as f64)
    }
}

nautilus_strategy!(VpinMarketMaker, {
    fn on_order_rejected(&mut self, event: OrderRejected) {
        self.pending_self_cancels.remove(&event.client_order_id);
        self.last_quoted_mid = None;
    }

    fn on_order_expired(&mut self, event: OrderExpired) {
        self.pending_self_cancels.remove(&event.client_order_id);
        self.last_quoted_mid = None;
    }

    fn on_order_filled(&mut self, event: &OrderFilled) {
        let closed = {
            let cache = self.cache();
            cache
                .order(&event.client_order_id)
                .is_some_and(|order| order.is_closed())
        };
        if closed {
            self.pending_self_cancels.remove(&event.client_order_id);
        }
    }

    fn on_order_canceled(&mut self, event: &OrderCanceled) {
        if self.pending_self_cancels.remove(&event.client_order_id) {
            return;
        }
        if self.config.on_cancel_resubmit {
            self.last_quoted_mid = None;
        }
    }
});

impl Debug for VpinMarketMaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(VpinMarketMaker))
            .field("config", &self.config)
            .field("trade_size", &self.trade_size)
            .field("vpin", &self.vpin)
            .field("signed_vpin", &self.signed_vpin)
            .finish()
    }
}

impl DataActor for VpinMarketMaker {
    fn on_start(&mut self) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        let (instrument, size_precision, min_quantity) = {
            let cache = self.cache();
            let instrument = cache.try_instrument(&instrument_id)?;
            let size_precision = instrument.size_precision();
            let min_quantity = instrument.min_quantity();
            (instrument, size_precision, min_quantity)
        };
        self.price_precision = Some(instrument.price_precision());
        self.instrument = Some(instrument);

        let trade_size = match self.trade_size {
            Some(qty) => Quantity::from_decimal_dp(qty.as_decimal(), size_precision)
                .map_err(|e| anyhow::anyhow!("Invalid trade_size precision: {e}"))?,
            None => min_quantity.unwrap_or_else(|| Quantity::new(1.0, size_precision)),
        };
        self.trade_size = Some(trade_size);

        self.subscribe_book_deltas(
            instrument_id,
            BookType::L2_MBP,
            NonZeroUsize::new(50),
            None,
            true,
            None,
        );

        self.subscribe_trades(instrument_id, None, None);

        self.clock().set_timer(
            "VPIN_MM_TIMER",
            Duration::from_secs(self.config.timer_interval_secs),
            None,
            None,
            None,
            None,
            None,
        )?;

        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        self.cancel_all_orders(instrument_id, None, None, None)?;
        self.close_all_positions(instrument_id, None, None, None, None, None, None, None)?;
        self.unsubscribe_book_deltas(instrument_id, None, None);
        self.unsubscribe_trades(instrument_id, None, None);
        Ok(())
    }

    fn on_trade(&mut self, tick: &TradeTick) -> anyhow::Result<()> {
        let size = tick.size.as_f64();
        match tick.aggressor_side {
            nautilus_model::enums::AggressorSide::Buyer => self.bucket_buy_volume += size,
            nautilus_model::enums::AggressorSide::Seller => self.bucket_sell_volume += size,
            _ => {}
        }
        Ok(())
    }

    fn on_book_deltas(&mut self, deltas: &OrderBookDeltas) -> anyhow::Result<()> {
        if deltas.instrument_id != self.config.instrument_id {
            return Ok(());
        }

        log::debug!("Book delta received: instrument={}", deltas.instrument_id);

        self.place_grid_orders()?;

        Ok(())
    }

    fn on_time_event(
        &mut self,
        _event: &nautilus_common::timer::TimeEvent,
    ) -> anyhow::Result<()> {
        log::debug!("Timer event received");
        self.place_grid_orders()?;
        Ok(())
    }

    fn on_reset(&mut self) -> anyhow::Result<()> {
        self.instrument = None;
        self.trade_size = self.config.trade_size;
        self.price_precision = None;
        self.last_quoted_mid = None;
        self.pending_self_cancels.clear();
        self.bucket_buy_volume = 0.0;
        self.bucket_sell_volume = 0.0;
        self.abs_imbalances.clear();
        self.signed_imbalances.clear();
        self.vpin = None;
        self.signed_vpin = None;
        Ok(())
    }
}
