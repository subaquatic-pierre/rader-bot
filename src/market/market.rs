use futures::StreamExt;

use serde::{Deserialize, Serialize};

use std::time::{Duration, SystemTime};
use std::{collections::HashMap, sync::Arc};

// use tokio::time::{self, Duration};

use crate::exchange::stream::build_stream_id;
use crate::exchange::types::{ApiResult, StreamType};
use crate::{
    exchange::{
        api::ExchangeApi,
        stream::{StreamManager, StreamMeta},
    },
    market::{
        kline::{Kline, KlineData, KlineMeta},
        messages::MarketMessage,
        ticker::{Ticker, TickerData, TickerMeta},
        types::ArcReceiver,
    },
    storage::manager::StorageManager,
    utils::{
        kline::generate_kline_filenames_in_range,
        time::{generate_ts, timestamp_to_datetime},
    },
};

use super::types::ArcMutex;

pub trait MarketDataSymbol {
    fn symbol(&self) -> String;
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MarketData {
    all_klines: HashMap<String, KlineData>,
    all_tickers: HashMap<String, TickerData>,
    storage_manager: StorageManager,
    last_backup: SystemTime,
}

const BACKUP_INTERVAL: u64 = 20;

impl MarketData {
    pub fn new(storage_manager: StorageManager) -> Self {
        Self {
            storage_manager,
            all_klines: HashMap::new(),
            all_tickers: HashMap::new(),
            last_backup: SystemTime::now(),
        }
    }

    pub fn add_kline(&mut self, kline: Kline) {
        // get kline key eg. BTCUSDT@kline_1m
        let kline_key = Self::build_kline_key(&kline.symbol, &kline.interval);

        // add new kline to data if key found for kline symbol
        if let Some(kline_data) = self.all_klines.get_mut(&kline_key) {
            kline_data.add_kline(kline);
        } else {
            // create new key for new kline eg. ETHUSDT@kline_1h
            let klines = vec![kline.clone()];
            let new_kline_data = KlineData {
                meta: KlineMeta::new(&kline.symbol, &kline.interval),
                klines,
            };
            self.all_klines
                .insert(kline_key.to_string(), new_kline_data);
        }

        // Save klines to disk if last backup more than 1 minute
        let time_elapsed = SystemTime::now()
            .duration_since(self.last_backup)
            .unwrap_or(Duration::from_secs(0));

        if time_elapsed >= Duration::from_secs(BACKUP_INTERVAL) {
            for (key, kline_data) in self.all_klines.iter() {
                let klines: Vec<Kline> = kline_data.klines.clone();

                self.storage_manager
                    .save_klines(&klines, key)
                    .expect("Unable to save Klines");
            }

            // Clear tickers from ticker_data
            for (_k, kline_data) in self.all_klines.iter_mut() {
                kline_data.clear_klines();
            }

            // Update the last backup time
            self.last_backup = SystemTime::now();
        }
    }

    pub fn update_ticker(&mut self, ticker: Ticker) {
        let ticker_key = Self::build_ticker_key(&ticker.symbol);
        let now = generate_ts();

        if let Some(ticker_data) = self.all_tickers.get_mut(&ticker_key) {
            ticker_data.update_ticker(ticker, now);
        } else {
            let new_ticker_data = TickerData {
                meta: TickerMeta::new(&ticker.symbol),
                ticker,
            };
            self.all_tickers
                .insert(ticker_key.to_string(), new_ticker_data);
        }
    }

    pub fn kline_data(
        &mut self,
        symbol: &str,
        interval: &str,
        from_ts: Option<u64>,
        to_ts: Option<u64>,
        limit: Option<usize>,
    ) -> Option<KlineData> {
        let kline_key = Self::build_kline_key(symbol, interval);

        let in_mem_kline = match self.all_klines.get(&kline_key) {
            Some(kline_data) => kline_data.klines.clone(),
            None => vec![],
        };

        // create filtered klines to hold all klines which are filtered
        let mut filtered_klines: Vec<Kline> = Vec::new();

        let filenames = match from_ts {
            Some(from_ts) => match to_ts {
                Some(to_ts) => Some(generate_kline_filenames_in_range(
                    &kline_key, from_ts, to_ts,
                )),
                None => Some(generate_kline_filenames_in_range(
                    &kline_key,
                    from_ts,
                    generate_ts(),
                )),
            },
            None => None,
        };

        if let Some(filenames) = filenames {
            for kline_filename in filenames {
                if let Some(klines) = self.storage_manager.load_klines(&kline_filename) {
                    filtered_klines.extend_from_slice(&klines);
                }
            }
        };

        filtered_klines.extend_from_slice(&in_mem_kline);

        // filtered by from_ts and to_ts
        if let Some(from_ts) = from_ts {
            filtered_klines.retain(|kline| kline.open_time >= from_ts);
            if let Some(to_ts) = to_ts {
                filtered_klines.retain(|kline| kline.open_time <= to_ts);
            }
        }

        // Sort the klines by open_time in descending order
        filtered_klines.sort_by(|a, b| a.open_time.cmp(&b.open_time));

        // append in mem klines

        // Limit the number of data points returned
        if let Some(limit) = limit {
            filtered_klines = filtered_klines[..limit].to_vec();
        }

        // Create a new KlineData object to hold the filtered klines
        let filtered_kline_data = KlineData {
            meta: KlineMeta {
                symbol: symbol.to_string(),
                interval: interval.to_string(),
                len: filtered_klines.len() as u64,
                last_update: generate_ts(),
            },
            klines: filtered_klines,
        };

        if filtered_kline_data.meta.len == 0 {
            None
        } else {
            Some(filtered_kline_data)
        }
    }

    /// return last 20 seconds of tickers for given symbol
    pub fn ticker_data(&self, symbol: &str) -> Option<TickerData> {
        let ticker_key = Self::build_ticker_key(symbol);
        if let Some(ticker_data) = self.all_tickers.get(&ticker_key) {
            return Some(ticker_data.clone());
        }

        None
    }

    pub fn build_kline_key(symbol: &str, interval: &str) -> String {
        format!("{}@kline_{}", symbol, interval)
    }

    pub fn build_ticker_key(symbol: &str) -> String {
        format!("{}@ticker", symbol)
    }

    pub fn build_kline_filename(kline_key: &str, timestamp: u64) -> String {
        let month_str = Self::build_kline_month_string(timestamp);
        format!("{kline_key}-{month_str}.csv")
    }

    pub fn build_kline_filename_from_year_month(kline_key: &str, year: u32, month: u32) -> String {
        format!("{kline_key}-{:04}-{:02}.csv", year, month)
    }

    pub fn build_kline_month_string(timestamp: u64) -> String {
        let timestamp = timestamp_to_datetime(timestamp);
        timestamp.format("%Y-%m").to_string()
    }
}

pub struct Market {
    market_receiver: ArcReceiver<MarketMessage>,
    // stream_manager: ArcMutex<StreamManager>,
    data: ArcMutex<MarketData>,
    exchange_api: Arc<Box<dyn ExchangeApi>>,
    needed_streams: ArcMutex<Vec<StreamMeta>>,
}

impl Market {
    pub async fn new(
        // stream_manager: ArcMutex<StreamManager>,
        market_receiver: ArcReceiver<MarketMessage>,
        exchange_api: Arc<Box<dyn ExchangeApi>>,
        storage_manager: StorageManager,
    ) -> Self {
        let mut _self = Self {
            data: ArcMutex::new(MarketData::new(storage_manager)),
            market_receiver,
            // stream_manager,
            exchange_api,
            needed_streams: ArcMutex::new(vec![]),
        };

        _self.init().await;

        _self
    }

    // ---
    // Data Methods
    // ---

    pub async fn last_price(&self, symbol: &str) -> Option<f64> {
        let ticker = self.data.lock().await.ticker_data(symbol);

        ticker.map(|ticker| ticker.ticker.last_price)
    }

    pub async fn kline_data(
        &self,
        symbol: &str,
        interval: &str,
        from_ts: Option<u64>,
        to_ts: Option<u64>,
        limit: Option<usize>,
    ) -> Option<KlineData> {
        self.data
            .lock()
            .await
            .kline_data(symbol, interval, from_ts, to_ts, limit)
    }

    pub async fn ticker_data(&self, symbol: &str) -> Option<TickerData> {
        self.data.lock().await.ticker_data(symbol)
    }

    pub async fn market_data(&self) -> MarketData {
        self.data.lock().await.clone()
    }

    // ---
    // Stream Methods
    // ---

    pub async fn active_streams(&self) -> Vec<StreamMeta> {
        self.exchange_api.active_streams().await
    }

    pub async fn open_stream(
        &self,
        stream_type: StreamType,
        symbol: &str,
        interval: Option<&str>,
    ) -> ApiResult<String> {
        self.exchange_api
            .open_stream(stream_type, symbol, interval)
            .await
    }

    pub async fn close_stream(&self, stream_id: &str) -> Option<StreamMeta> {
        self.exchange_api.close_stream(stream_id).await
    }

    // ---
    // Init methods
    // ---

    async fn init(&self) {
        // Add initial needed streams
        self.add_needed_stream("BTC-USDT", StreamType::Ticker, None)
            .await;

        self.init_market_receivers().await;
        self.init_active_stream_monitor().await;
    }

    async fn init_market_receivers(&self) {
        let market_receiver = self.market_receiver.clone();
        let market_data = self.data.clone();

        // let active_streams = self.active_streams.clone();

        // spawn thread to handle stream_manager messages
        tokio::spawn(async move {
            while let Some(message) = market_receiver.lock().await.recv().await {
                // println!("{message:?}");

                match message {
                    MarketMessage::UpdateKline(kline) => {
                        market_data.lock().await.add_kline(kline);
                    }
                    MarketMessage::UpdateTicker(ticker) => {
                        market_data.lock().await.update_ticker(ticker);
                    }
                }
            }
        });
    }

    async fn init_active_stream_monitor(&self) {
        let stream_manager = self.exchange_api.get_stream_manager();
        let exchange_api = self.exchange_api.clone();
        let needed_streams = self.needed_streams.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let active_streams = stream_manager.lock().await.active_streams().await;
                for needed_stream_meta in needed_streams.lock().await.iter() {
                    let active_stream_meta = active_streams
                        .iter()
                        .find(|&meta| meta.symbol == needed_stream_meta.symbol);

                    match active_stream_meta {
                        Some(_meta) => {
                            continue;
                        }
                        None => {
                            let need_stream = needed_stream_meta.clone();

                            let _ = exchange_api
                                .open_stream(
                                    needed_stream_meta.stream_type.clone(),
                                    &needed_stream_meta.symbol,
                                    need_stream.interval.as_deref(),
                                )
                                .await;
                        }
                    }
                }
            }
        });
    }

    pub async fn add_needed_stream(
        &self,
        symbol: &str,
        stream_type: StreamType,
        interval: Option<&str>,
    ) {
        let mut needed_streams = self.needed_streams.lock().await;
        let url = self
            .exchange_api
            .build_stream_url(symbol, stream_type, interval);
        let stream_id = build_stream_id(symbol, interval);
        let btc_stream_meta = StreamMeta::new(&stream_id, &url, symbol, StreamType::Ticker, None);

        needed_streams.push(btc_stream_meta);
    }

    pub async fn remove_needed_stream(
        &self,
        symbol: &str,
        _stream_type: StreamType,
        interval: Option<&str>,
    ) {
        let mut needed_streams = self.needed_streams.lock().await;
        let stream_id = build_stream_id(symbol, interval);

        needed_streams.retain(|x| x.id != stream_id);
    }
}
