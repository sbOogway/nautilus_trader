#![allow(non_snake_case, mixed_script_confusables)]

//! `avellaneda_stoikov` - Avellaneda-Stoikov market making strategy

use chrono::{TimeZone, Utc};
use nautilus_backtest::{
    config::{
        BacktestDataConfig, BacktestRunConfig, BacktestVenueConfig, NautilusDataType,
    },
    node::BacktestNode,
};
use nautilus_bin::config::Config;
use nautilus_bin::exchange::Exchange;
use nautilus_bin::strategy::avellaneda_stoikov::{
    config::AvellanedaStoikovConfig,
    strategy::AvellanedaStoikov,
};
use nautilus_common::enums::Environment;
use nautilus_model::{
    enums::{AccountType, BookType, OmsType},
    identifiers::{AccountId, InstrumentId, TraderId},
    types::Quantity,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    nautilus_common::logging::ensure_logging_initialized();

    let cfg = Config::load("config.toml".to_string())?
        .avellaneda_stoikov
        .expect("config.toml missing [avellaneda_stoikov] section");

    let exchange: Exchange = cfg.exchange.parse()?;
    let trader_id = TraderId::from(cfg.trader_id.as_str());
    let instrument_id = InstrumentId::from(cfg.instrument_id);
    let catalog_path = cfg.path.clone();

    let config = AvellanedaStoikovConfig::builder()
        .instrument_id(instrument_id)
        .trade_size(Quantity::from(cfg.trade_size.as_str()))
        .gamma(cfg.gamma)
        .sigma(cfg.sigma)
        .kappa(cfg.kappa)
        .arrival_rate(cfg.arrival_rate)
        .time_horizon_secs(cfg.time_horizon_secs)
        .lookback_secs(cfg.lookback_secs)
        .maybe_expire_time_secs(cfg.expire_time_secs)
        .build();

    let start_date = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).single().unwrap();
    let end_date = Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, 0).single().unwrap();

    match &cfg.execution_environment {
        Environment::Backtest => {
            let venue = BacktestVenueConfig::builder()
                .name("BYBIT")
                .oms_type(OmsType::Hedging)
                .account_type(AccountType::Margin)
                .book_type(BookType::L2_MBP)
                .starting_balances(vec!["1_000 USDT".to_string()])
                .build()?;

            let order_book = BacktestDataConfig::builder()
                .catalog_path(catalog_path.clone())
                .instrument_id(instrument_id)
                .start_time(start_date.into())
                .end_time(end_date.into())
                .data_type(NautilusDataType::OrderBookDelta)
                .optimize_file_loading(true)
                .build()?;

            let trades = BacktestDataConfig::builder()
                .catalog_path(catalog_path.clone())
                .instrument_id(instrument_id)
                .start_time(start_date.into())
                .end_time(end_date.into())
                .data_type(NautilusDataType::TradeTick)
                .optimize_file_loading(true)
                .build()?;

            let run = BacktestRunConfig::builder()
                .id("as-backtest".to_string())
                .venues(vec![venue])
                .data(vec![order_book, trades])
                .chunk_size(1_000_000)
                .build()?;

            let mut node = BacktestNode::new(vec![run])?;

            node.build()?;
            {
                let engine = node.get_engine_mut("as-backtest").unwrap();
                let strategy = AvellanedaStoikov::new(config);
                engine.add_strategy(strategy)?;
            }
            node.run()?;

            let engine = node.get_engine_mut("as-backtest").unwrap();
            let snapshots = engine
                .kernel()
                .portfolio
                .borrow()
                .snapshots(&AccountId::from("BYBIT-001"));
            log::info!("{snapshots:#?}");
        }
        Environment::Sandbox => todo!(),
        Environment::Live => {
            let mut node = exchange.build_node(trader_id)?;
            let strategy = AvellanedaStoikov::new(config);
            node.add_strategy(strategy)?;
            node.run().await?;
        }
    }

    Ok(())
}
