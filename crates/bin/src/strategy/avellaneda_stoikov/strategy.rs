//! Avellaneda-Stoikov market making strategy implementation.
//!
//! Follows "High-frequency trading in a limit order book"
//! by Avellaneda & Stoikov (2008), infinite-horizon formulation.

use std::{collections::VecDeque, fmt::Debug, num::NonZeroUsize, time::Duration};

use ahash::AHashSet;
use nautilus_common::actor::DataActor;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::TradeTick,
    enums::{BookType, OrderSide, TimeInForce},
    events::{OrderCanceled, OrderExpired, OrderFilled, OrderRejected},
    identifiers::ClientOrderId,
    instruments::{Instrument, InstrumentAny},
    orders::Order,
    types::{Price, Quantity},
};
use nautilus_trading::{Strategy, StrategyCore, nautilus_strategy};

use crate::strategy::avellaneda_stoikov::config::AvellanedaStoikovConfig;

#[allow(non_snake_case)]
pub struct AvellanedaStoikov {
    pub(super) core: StrategyCore,
    pub(super) config: AvellanedaStoikovConfig,
    pub(super) instrument: Option<InstrumentAny>,
    pub(super) trade_size: Option<Quantity>,
    pub(super) price_precision: Option<u8>,
    pub(super) last_quoted_mid: Option<Price>,
    pub(super) trade_returns: VecDeque<(UnixNanos, f64)>,
    pub(super) last_trade_price: Option<f64>,
    pub(super) pending_self_cancels: AHashSet<ClientOrderId>,
}

impl AvellanedaStoikov {
    #[must_use]
    #[allow(dead_code)]
    pub fn new(config: AvellanedaStoikovConfig) -> Self {
        Self {
            core: StrategyCore::new(config.base.clone()),
            instrument: None,
            trade_size: config.trade_size,
            config,
            price_precision: None,
            last_quoted_mid: None,
            trade_returns: VecDeque::new(),
            last_trade_price: None,
            pending_self_cancels: AHashSet::new(),
        }
    }

    fn estimate_sigma(&self) -> f64 {
        let returns: Vec<f64> = self
            .trade_returns
            .iter()
            .map(|(_, r)| *r)
            .filter(|r| r.is_finite() && r.abs() < 0.05)
            .collect();
        if returns.len() < 2 {
            return self.config.sigma;
        }
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let lookback_secs = self.config.lookback_secs as f64;
        let returns_per_year = n * 31_536_000.0 / lookback_secs;
        (variance.sqrt() * returns_per_year.sqrt()).clamp(1e-4, 5.0)
    }

    fn reservation_price(&self, mid: f64, q: f64, sigma: f64) -> f64 {
        let tau = self.config.time_horizon_secs / 31_536_000.0;
        let raw = 1.0 - q * self.config.gamma * sigma.powi(2) * tau;
        mid * raw.clamp(0.5, 1.5)
    }

    fn optimal_spread(&self, sigma: f64) -> f64 {
        let tau = self.config.time_horizon_secs / 31_536_000.0;
        let gamma = self.config.gamma;
        let kappa = self.config.kappa;
        gamma * sigma.powi(2) * tau + (2.0 / gamma) * (1.0 + gamma / kappa).ln()
    }

    fn prune_old_returns(&mut self, now: UnixNanos) {
        let cutoff = now.as_u64().saturating_sub(self.config.lookback_secs * 1_000_000_000);
        while let Some(&(ts, _)) = self.trade_returns.front() {
            if ts.as_u64() < cutoff {
                self.trade_returns.pop_front();
            } else {
                break;
            }
        }
    }
}

nautilus_strategy!(AvellanedaStoikov, {
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
        self.pending_self_cancels.remove(&event.client_order_id);
    }
});

impl Debug for AvellanedaStoikov {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvellanedaStoikov")
            .field("config", &self.config)
            .field("trade_size", &self.trade_size)
            .finish()
    }
}

impl DataActor for AvellanedaStoikov {
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
            "AS_TIMER",
            Duration::from_secs(5),
            None,
            None,
            None,
            None,
            None,
        )?;

        Ok(())
    }

    fn on_trade(&mut self, tick: &TradeTick) -> anyhow::Result<()> {
        let price = tick.price.as_f64();
        if let Some(last) = self.last_trade_price {
            let log_return = (price / last).ln();
            self.trade_returns.push_back((tick.ts_event, log_return));
        }
        self.last_trade_price = Some(price);
        Ok(())
    }

    fn on_time_event(
        &mut self,
        _event: &nautilus_common::timer::TimeEvent,
    ) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        let strategy_id = self.strategy_id().expect("Strategy must be registered");
        let price_precision = self.price_precision.ok_or_else(|| {
            anyhow::anyhow!("Cannot requote: price_precision is not resolved")
        })?;

        let now = self.clock().timestamp_ns();
        self.prune_old_returns(now);

        let sigma = self.estimate_sigma();

        let order_book = self
            .cache()
            .order_book(&instrument_id)
            .ok_or_else(|| anyhow::anyhow!("Order book not found for {instrument_id}"))?;
        let (Some(bid_price), Some(ask_price)) =
            (order_book.best_bid_price(), order_book.best_ask_price())
        else {
            return Ok(());
        };
        let mid_f64 = f64::midpoint(bid_price.as_f64(), ask_price.as_f64());

        let q_f64: f64 = {
            let instrument_id = Some(&instrument_id);
            let strategy = Some(&strategy_id);
            let cache = self.cache();
            cache
                .positions_open(None, instrument_id, strategy, None, None)
                .iter()
                .map(|p| p.signed_qty)
                .sum()
        };

        let r = self.reservation_price(mid_f64, q_f64, sigma);
        let spread = self.optimal_spread(sigma);

        let half_spread = mid_f64 * spread / 2.0;
        let bid_f64 = r - half_spread;
        let ask_f64 = r + half_spread;

        if !bid_f64.is_finite() || !ask_f64.is_finite() || bid_f64 <= 0.0 || ask_f64 <= 0.0 {
            log::warn!("AS: invalid quotes bid={bid_f64} ask={ask_f64}; skipping requote");
            return Ok(());
        }

        let Some(instrument) = self.instrument.as_ref() else {
            anyhow::bail!("Instrument not resolved");
        };

        // Post-only constraint: never quote at or through the opposite side of
        // the current book, even if it is momentarily crossed (bid > ask).
        let tick = instrument.price_increment().as_f64();
        let best_bid = bid_price.as_f64();
        let best_ask = ask_price.as_f64();
        let bid_f64 = bid_f64.min(best_ask - tick);
        let ask_f64 = ask_f64.max(best_bid + tick);

        let bid_price = instrument.next_bid_price(bid_f64, 0).ok_or_else(|| {
            anyhow::anyhow!("Cannot compute valid bid price from {bid_f64}")
        })?;
        let ask_price = instrument.next_ask_price(ask_f64, 0).ok_or_else(|| {
            anyhow::anyhow!("Cannot compute valid ask price from {ask_f64}")
        })?;

        log::info!(
            "AS: mid={mid_f64:.4} q={q_f64} sigma={sigma:.6} r={r:.4} spread_bps={:.1} bid={bid_price} ask={ask_price}",
            spread * 10_000.0
        );

        let open_count = {
            let cache = self.cache();
            let venue = Some(&instrument_id.venue);
            let inst = Some(&instrument_id);
            let sid = Some(&strategy_id);
            cache.orders_open_count(venue, inst, sid, None, None)
                + cache.orders_inflight_count(venue, inst, sid, None, None)
        };

        if open_count > 0 {
            let ids: Vec<ClientOrderId> = {
                let cache = self.cache();
                let inst = Some(&instrument_id);
                let strategy = Some(&strategy_id);
                let open = cache.orders_open(None, inst, strategy, None, None);
                let inflight = cache.orders_inflight(None, inst, strategy, None, None);
                open.iter()
                    .chain(inflight.iter())
                    .map(|order| order.client_order_id())
                    .collect()
            };
            self.pending_self_cancels.extend(ids);
        }

        self.cancel_all_orders(instrument_id, None, None, None)?;

        let trade_size = self
            .trade_size
            .ok_or_else(|| anyhow::anyhow!("trade_size not resolved"))?;

        let (tif, expire_time) = match self.config.expire_time_secs {
            Some(secs) => {
                let now_ns = self.clock().timestamp_ns();
                let expire_ns = now_ns + secs * 1_000_000_000;
                (Some(TimeInForce::Gtd), Some(expire_ns))
            }
            None => (None, None),
        };

        let bid_order = self.order().limit(
            instrument_id,
            OrderSide::Buy,
            trade_size,
            bid_price,
            tif,
            expire_time,
            Some(true),
            None, None, None, None, None, None, None, None, None,
        );
        self.submit_order(bid_order, None, None, None)?;

        let ask_order = self.order().limit(
            instrument_id,
            OrderSide::Sell,
            trade_size,
            ask_price,
            tif,
            expire_time,
            Some(true),
            None, None, None, None, None, None, None, None, None,
        );
        self.submit_order(ask_order, None, None, None)?;

        self.last_quoted_mid = Some(Price::new(mid_f64, price_precision));
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        let instrument_id = self.config.instrument_id;
        self.cancel_all_orders(instrument_id, None, None, None)?;
        self.close_all_positions(instrument_id, None, None, None, None, None, None, None)?;
        self.unsubscribe_book_deltas(instrument_id, None, None);
        Ok(())
    }

    fn on_reset(&mut self) -> anyhow::Result<()> {
        self.instrument = None;
        self.trade_size = self.config.trade_size;
        self.price_precision = None;
        self.last_quoted_mid = None;
        self.trade_returns.clear();
        self.last_trade_price = None;
        self.pending_self_cancels.clear();
        Ok(())
    }
}
