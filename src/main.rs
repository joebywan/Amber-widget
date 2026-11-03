// Desktop dashboard for Amber Electric prices and grid usage (solar + battery homes).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod tray;

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeDelta, Timelike, Utc};
use eframe::egui::{self, Align2, Color32, FontId, RichText, Sense, Shape, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
const BRACKET_SECONDS: i64 = 300;
// Amber allows about 50 calls per 5 minutes per account. The worst case below (a price that never
// appears, nothing learned yet) is 30 fast checks plus 18 slow ones.
const FAST_POLL_MS: i64 = 1000;
const SLOW_POLL_MS: i64 = 15_000;
const MIN_DELAY_SAMPLES: usize = 5;
const RECENT_DELAY_SAMPLES: usize = 6;
const MAX_DELAY_SAMPLES: usize = 100;
const ERROR_RETRY_SECONDS: i64 = 60;
const USAGE_PERIOD: Duration = Duration::from_secs(3600);
const BUY_COLOR: Color32 = Color32::from_rgb(0x2a, 0x78, 0xc9);
const SELL_COLOR: Color32 = Color32::from_rgb(0xc9, 0x63, 0x1a);

// --- API shapes ---

#[derive(Deserialize)]
struct Site {
    id: String,
    status: String,
}

#[derive(Deserialize, Clone, Copy)]
struct AdvancedPrice {
    low: f64,
    predicted: f64,
    high: f64,
}

#[derive(Deserialize, Clone)]
struct TariffInformation {
    period: Option<String>,
    season: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Interval {
    #[serde(rename = "type")]
    kind: String,
    duration: i64,
    per_kwh: f64,
    spot_per_kwh: f64,
    renewables: f64,
    channel_type: String,
    spike_status: String,
    descriptor: String,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    #[serde(default)]
    estimate: bool,
    advanced_price: Option<AdvancedPrice>,
    tariff_information: Option<TariffInformation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageRow {
    date: String,
    channel_type: String,
    kwh: f64,
    cost: f64,
}

// --- Pure logic ---

fn forecast_cents(interval: &Interval) -> f64 {
    // Amber's advanced 'predicted' price is its best forecast; perKwh is the fallback.
    interval.advanced_price.map_or(interval.per_kwh, |p| p.predicted)
}

fn earn_cents(feed_in_cents: f64) -> f64 {
    // Amber reports feed-in as a price you pay, so negative means you get paid.
    -feed_in_cents
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    let index = (ordered.len() as f64 * fraction) as usize;
    ordered[index.min(ordered.len() - 1)]
}

/// One line of guidance. buy/sell are c/kWh now (sell = what you earn); buys/sells are forecasts.
fn advice(buy: f64, sell: Option<f64>, buys: &[f64], sells: &[f64], spike_status: &str, export_charge: bool) -> &'static str {
    let cheapest = buys.iter().copied().fold(f64::INFINITY, f64::min);
    if spike_status == "spike" {
        return "Price spike: avoid grid power, run off the battery or export it.";
    }
    if sell.is_some_and(|s| s < 0.0) {
        return "Exporting costs you money: soak up solar with the battery and loads.";
    }
    if export_charge {
        return "Export charge active: store solar in the battery rather than exporting for now.";
    }
    if buy <= 0.0 {
        return "You're paid to use grid power: charge the battery and run big loads.";
    }
    if let Some(s) = sell
        && !sells.is_empty()
        && !buys.is_empty()
        && s >= percentile(sells, 0.75)
        && s > cheapest
    {
        return "Good export price: selling beats what recharging later should cost.";
    }
    if !buys.is_empty() && buy <= percentile(buys, 0.25) {
        return "Cheap grid power compared to the next 24h: good time for big loads or charging.";
    }
    "Nothing unusual: let solar and the battery cover the house."
}

#[derive(Default, Debug, PartialEq, Clone, Copy)]
struct Day {
    bought: f64,
    sold: f64,
    net_dollars: f64,
}

fn usage_by_day(rows: &[UsageRow]) -> BTreeMap<String, Day> {
    let mut days: BTreeMap<String, Day> = BTreeMap::new();
    for row in rows {
        let day = days.entry(row.date.clone()).or_default();
        if row.channel_type == "feedIn" {
            day.sold += row.kwh.abs();
        } else {
            day.bought += row.kwh.abs();
        }
        day.net_dollars += row.cost / 100.0;
    }
    days
}

/// Tariff amount in the feed-in price beyond the market price: negative is an export charge, positive a
/// bonus. Feed-in is normally spot less network losses, so anything else came from the tariff.
fn export_adjustment(earn: f64, spot: f64) -> Option<f64> {
    // ponytail: 0.9593 is measured on tariff 96200; the tolerance absorbs drift, recheck if labels misfire.
    let adjustment = earn - 0.9593 * spot;
    (adjustment.abs() >= 1.0 + 0.01 * spot.abs()).then_some(adjustment)
}

fn period_of(interval: &Interval) -> Option<&str> {
    interval.tariff_information.as_ref()?.period.as_deref()
}

/// "Peak until 8:00pm", using the first forecast interval in a different period.
fn tariff_line(current: &Interval, rows: &[Interval]) -> String {
    let Some(period) = period_of(current) else { return String::new() };
    let mut name = describe(period);
    if let Some(first) = name.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    let change = rows.iter().find(|i| i.kind == "ForecastInterval" && period_of(i) != Some(period));
    let season = current.tariff_information.as_ref().and_then(|t| t.season.as_deref()).map(describe);
    let season = season.map_or(String::new(), |s| format!(" ({s} rates)"));
    match change {
        Some(i) => format!("{name} until {}{season}", clock(i.start_time.with_timezone(&Local))),
        None => format!("{name}{season}"),
    }
}

/// Explains any export charge or bonus inside the sell price: the current one, else the next one coming.
fn export_line(current: Option<f64>, rows: &[Interval]) -> String {
    let noun = |amount: f64| if amount < 0.0 { "export charge" } else { "export bonus" };
    let forecast: Vec<_> = rows
        .iter()
        .filter(|i| i.kind == "ForecastInterval")
        .map(|i| (i.start_time.with_timezone(&Local), export_adjustment(earn_cents(i.per_kwh), i.spot_per_kwh)))
        .collect();
    if let Some(amount) = current {
        let until = forecast.iter().find(|(_, a)| a.is_none());
        let until = until.map_or(String::new(), |(t, _)| format!(" until {}", clock(*t)));
        return format!("Sell includes a {:.1}c {}{until}", amount.abs(), noun(amount));
    }
    match forecast.iter().find_map(|(t, a)| a.map(|a| (*t, a))) {
        Some((t, amount)) => format!("{:.1}c {} from {}", amount.abs(), noun(amount), clock(t)),
        None => String::new(),
    }
}

/// Totals per "YYYY-MM", newest first.
fn month_totals(days: &BTreeMap<String, Day>) -> Vec<(String, Day)> {
    let mut months: BTreeMap<String, Day> = BTreeMap::new();
    for (date, day) in days {
        let month = months.entry(date.chars().take(7).collect()).or_default();
        month.bought += day.bought;
        month.sold += day.sold;
        month.net_dollars += day.net_dollars;
    }
    months.into_iter().rev().collect()
}

/// First current or forecast interval flagged as a spike or potential spike.
fn next_spike(intervals: &[Interval]) -> Option<&Interval> {
    intervals.iter().find(|i| i.kind != "ActualInterval" && i.spike_status != "none")
}

fn clock(moment: DateTime<Local>) -> String {
    moment.format("%-I:%M%P").to_string()
}

fn describe(descriptor: &str) -> String {
    let mut out = String::new();
    for c in descriptor.chars() {
        if c.is_uppercase() {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn dollars(value: f64) -> String {
    if value < 0.0 { format!("-${:.2}", -value) } else { format!("${value:.2}") }
}

fn channel(intervals: &[Interval], name: &str) -> Vec<Interval> {
    intervals.iter().filter(|i| i.channel_type == name).cloned().collect()
}

/// Start of the next 5-minute NEM bracket (brackets sit on multiples of 5 minutes in UTC).
fn next_bracket(now: DateTime<Utc>) -> DateTime<Utc> {
    let seconds = now.timestamp();
    DateTime::from_timestamp(seconds - seconds.rem_euclid(BRACKET_SECONDS) + BRACKET_SECONDS, 0).unwrap_or(now)
}

/// The delays that steer the checks: the last half hour, so a change in Amber's timing shows up quickly.
fn recent_delays(delays: &[f64]) -> &[f64] {
    &delays[delays.len().saturating_sub(RECENT_DELAY_SAMPLES)..]
}

/// Milliseconds after a bracket starts to make the first check, just ahead of Amber's usual publishing time.
fn first_poll_ms(delays: &[f64]) -> i64 {
    if delays.len() < MIN_DELAY_SAMPLES {
        return 0;
    }
    ((percentile(recent_delays(delays), 0.25) * 1000.0) as i64 - FAST_POLL_MS).max(0)
}

/// When the price counts as unusually late, in milliseconds into the bracket.
fn fast_until_ms(delays: &[f64]) -> i64 {
    if delays.len() < MIN_DELAY_SAMPLES {
        return 30_000;
    }
    (percentile(recent_delays(delays), 0.9) * 1000.0) as i64 + 5000
}

/// Gap until the next check: fast while the price is due, slower once it's unusually late.
fn poll_gap_ms(delays: &[f64], ms_into_bracket: i64) -> i64 {
    if ms_into_bracket < fast_until_ms(delays) { FAST_POLL_MS } else { SLOW_POLL_MS }
}

/// Delay to learn from a check that found a new bracket. Amber published between the last miss and this
/// check, so take the midpoint. If the very first check found it, the real delay could be anything
/// earlier, so aim well below it: a real move earlier is then found in a few brackets, not hours.
fn delay_sample(found: f64, last_miss: Option<f64>) -> f64 {
    last_miss.map_or(found / 2.0, |miss| (found + miss) / 2.0).max(0.0)
}

fn add_delay(delays: &mut Vec<f64>, sample: f64) {
    delays.push(sample);
    if delays.len() > MAX_DELAY_SAMPLES {
        delays.remove(0);
    }
}

/// Returns (should alert, is above). Settings once per crossing, and re-arms when the price drops back.
fn crossed(value: Option<f64>, limit: Option<f64>, was_above: bool) -> (bool, bool) {
    let above = matches!((value, limit), (Some(v), Some(l)) if v > l);
    (above && !was_above, above)
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
enum Sound {
    Message,
    Bell,
    Alarm,
    Warning,
}

impl Sound {
    const ALL: [Sound; 4] = [Sound::Message, Sound::Bell, Sound::Alarm, Sound::Warning];

    fn label(self) -> &'static str {
        match self {
            Sound::Message => "Message sound",
            Sound::Bell => "Bell sound",
            Sound::Alarm => "Alarm sound",
            Sound::Warning => "Warning sound",
        }
    }

    /// Name in the freedesktop sound theme.
    fn theme_name(self) -> &'static str {
        match self {
            Sound::Message => "message-new-instant",
            Sound::Bell => "bell",
            Sound::Alarm => "alarm-clock-elapsed",
            Sound::Warning => "dialog-warning",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(default)]
struct Settings {
    buy_on: bool,
    buy: f64,
    sell_on: bool,
    sell: f64,
    buy_below_on: bool,
    buy_below: f64,
    sell_forecast_on: bool,
    sell_forecast: f64,
    sound_on: bool,
    sound: Sound,
    tray: tray::Show,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            buy_on: false,
            buy: 50.0,
            sell_on: false,
            sell: 20.0,
            buy_below_on: false,
            buy_below: 10.0,
            sell_forecast_on: false,
            sell_forecast: 30.0,
            sound_on: true,
            sound: Sound::Message,
            tray: tray::Show::Buy,
        }
    }
}

impl Settings {
    fn buy_limit(&self) -> Option<f64> {
        self.buy_on.then_some(self.buy)
    }

    fn sell_limit(&self) -> Option<f64> {
        self.sell_on.then_some(self.sell)
    }

    fn buy_below_limit(&self) -> Option<f64> {
        self.buy_below_on.then_some(self.buy_below)
    }

    fn sell_forecast_limit(&self) -> Option<f64> {
        self.sell_forecast_on.then_some(self.sell_forecast)
    }
}

// --- What the window shows, built from API responses ---

struct Point {
    start: DateTime<Local>,
    end: DateTime<Local>,
    value: f64,
    range: Option<(f64, f64)>,
    forecast: bool,
    peak: bool,
    export_adjustment: Option<f64>,
}

type Series = Vec<Point>;

struct Prices {
    buy: f64,
    buy_note: String,
    sell: Option<f64>,
    renewables: f64,
    advice: &'static str,
    spike: Option<(DateTime<Utc>, String)>,
    windows: String,
    tariff: String,
    export: String,
    best_sell: Option<(DateTime<Utc>, f64)>,
    chart_buy: Series,
    chart_sell: Series,
}

fn build_prices(now: &[Interval], chart: &[Interval]) -> Result<Prices, String> {
    let buy_now = channel(now, "general").into_iter().next().ok_or("Amber sent no general price")?;
    let sell_now = channel(now, "feedIn").into_iter().next();
    let sell = sell_now.as_ref().map(|i| earn_cents(i.per_kwh));
    let adjustment = sell_now.as_ref().and_then(|i| export_adjustment(earn_cents(i.per_kwh), i.spot_per_kwh));
    let buy_rows = channel(chart, "general");
    let sell_rows = channel(chart, "feedIn");
    let forecast = |rows: &[Interval], convert: fn(f64) -> f64| -> Vec<(DateTime<Utc>, f64)> {
        rows.iter()
            .filter(|i| i.kind == "ForecastInterval")
            .map(|i| (i.start_time, convert(forecast_cents(i))))
            .collect()
    };
    let buys = forecast(&buy_rows, |v| v);
    let sells = forecast(&sell_rows, earn_cents);
    let values = |points: &[(DateTime<Utc>, f64)]| points.iter().map(|p| p.1).collect::<Vec<_>>();
    let at = |t: DateTime<Utc>| clock(t.with_timezone(&Local));

    let mut windows = Vec::new();
    if let Some(p) = buys.iter().min_by(|a, b| a.1.total_cmp(&b.1)) {
        windows.push(format!("Cheapest buy next 24h: {:.1}c at {}", p.1, at(p.0)));
    }
    if let Some(p) = sells.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
        windows.push(format!("Best sell: {:.1}c at {}", p.1, at(p.0)));
    }

    let spike_interval = next_spike(&buy_rows).or((buy_now.spike_status != "none").then_some(&buy_now));
    let spike = spike_interval.map(|i| {
        let kind = if i.spike_status == "spike" { "Spike" } else { "Possible spike" };
        (i.start_time, format!("{kind} from {}: {:.0}c/kWh", at(i.start_time), forecast_cents(i)))
    });

    let series = |rows: &[Interval], convert: fn(f64) -> f64| -> Series {
        rows.iter()
            .map(|i| {
                let forecast = i.kind == "ForecastInterval";
                let (a, b) = i.advanced_price.map_or((None, None), |p| (Some(convert(p.low)), Some(convert(p.high))));
                Point {
                    start: i.start_time.with_timezone(&Local),
                    end: i.end_time.with_timezone(&Local),
                    value: convert(if forecast { forecast_cents(i) } else { i.per_kwh }),
                    range: a.zip(b).filter(|_| forecast).map(|(a, b)| (a.min(b), a.max(b))),
                    forecast,
                    peak: period_of(i) == Some("peak"),
                    export_adjustment: export_adjustment(earn_cents(i.per_kwh), i.spot_per_kwh),
                }
            })
            .collect()
    };

    Ok(Prices {
        buy: buy_now.per_kwh,
        buy_note: describe(&buy_now.descriptor) + if buy_now.estimate { ", estimate" } else { "" },
        sell,
        renewables: buy_now.renewables,
        advice: advice(buy_now.per_kwh, sell, &values(&buys), &values(&sells), &buy_now.spike_status, adjustment.is_some_and(|a| a < 0.0)),
        spike,
        windows: windows.join(" · "),
        tariff: tariff_line(&buy_now, &buy_rows),
        export: export_line(adjustment, &sell_rows),
        best_sell: sells.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1)),
        chart_buy: series(&buy_rows, |v| v),
        chart_sell: series(&sell_rows, earn_cents),
    })
}

fn tray_status(prices: &Prices, show: tray::Show) -> tray::Status {
    let spike = prices.spike.as_ref().is_some_and(|(start, _)| *start <= Utc::now());
    tray::Status { buy: prices.buy, sell: prices.sell, spike, show }
}

fn send_alert(message: &str, settings: &Settings) -> Result<(), String> {
    let mut notification = notify_rust::Notification::new();
    notification.summary("Amber").body(message);
    if settings.sound_on {
        // ponytail: the notification daemon plays it, so a daemon without sound support stays silent.
        notification.sound_name(settings.sound.theme_name());
    }
    notification.show().map(|_| ()).map_err(|e| e.to_string())
}

// --- Token storage and HTTP ---

fn token_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("amber-widget").join("token")
}

// Keeps its old name so existing alert settings survive.
fn settings_path() -> PathBuf {
    token_path().with_file_name("alerts.json")
}

fn read_settings() -> Settings {
    fs::read_to_string(settings_path()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save_settings(settings: &Settings) -> std::io::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string(settings)?)
}

fn delays_path() -> PathBuf {
    token_path().with_file_name("delays.json")
}

fn read_delays() -> Vec<f64> {
    fs::read_to_string(delays_path()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn read_token() -> String {
    fs::read_to_string(token_path()).map(|t| t.trim().to_string()).unwrap_or_default()
}

fn save_token(token: &str) -> std::io::Result<()> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(token.as_bytes())
}

enum FetchError {
    Unauthorized,
    Other(String),
}

fn get<T: serde::de::DeserializeOwned>(path: &str, params: &[(&str, String)]) -> Result<T, FetchError> {
    let base = std::env::var("AMBER_API_BASE").unwrap_or_else(|_| "https://api.amber.com.au/v1".into());
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let mut request = agent.get(format!("{base}{path}")).header("Authorization", format!("Bearer {}", read_token()));
    for (name, value) in params {
        request = request.query(*name, value);
    }
    match request.call() {
        Ok(mut response) => response.body_mut().read_json().map_err(|e| FetchError::Other(e.to_string())),
        Err(ureq::Error::StatusCode(401 | 403)) => Err(FetchError::Unauthorized),
        Err(ureq::Error::StatusCode(code)) => Err(FetchError::Other(format!("Amber returned HTTP {code}"))),
        Err(e) => Err(FetchError::Other(e.to_string())),
    }
}

// --- Background worker: owns all polling, so alerts still fire when the window is minimised ---

enum Update {
    Prices(Box<Prices>),
    Usage(BTreeMap<String, Day>),
    Done(Option<String>),
    NeedToken(&'static str),
    Waiting,
    Timing(Timing),
}

/// What the Settings tab shows about when the worker checks for a new price. All in seconds into a bracket.
struct Timing {
    recent: Vec<f64>,
    stored: usize,
    first_check: f64,
    fast_until: f64,
    checks: u32,
}

fn timing(delays: &[f64], checks: u32) -> Timing {
    Timing {
        recent: recent_delays(delays).iter().rev().copied().collect(),
        stored: delays.len(),
        first_check: first_poll_ms(delays) as f64 / 1000.0,
        fast_until: fast_until_ms(delays) as f64 / 1000.0,
        checks,
    }
}

struct Worker {
    site: Option<String>,
    usage_at: Option<Instant>,
    alerted: HashSet<DateTime<Utc>>,
    settings: Arc<Mutex<Settings>>,
    buy_above: bool,
    sell_above: bool,
    buy_below: bool,
    sell_forecast_above: bool,
    tray: Sender<tray::Status>,
    usage_days: BTreeMap<String, Day>,
    shown_end: Option<DateTime<Utc>>,
    delays: Vec<f64>,
    checks_this_bracket: u32,
    last_miss: Option<DateTime<Utc>>,
    on_schedule: bool,
    send: Box<dyn Fn(Update) + Send>,
}

impl Worker {
    /// Returns whether Amber is showing the current bracket yet. Only `prices/current` is called while
    /// waiting for a new bracket; `forced` (the Refresh button) always reloads everything.
    fn cycle(&mut self, forced: bool) -> Result<bool, FetchError> {
        let site = match &self.site {
            Some(id) => id.clone(),
            None => {
                // ponytail: uses the first active site, add a picker if an account ever has several.
                let sites: Vec<Site> = get("/sites", &[])?;
                let first = sites.iter().find(|s| s.status == "active").or(sites.first());
                let id = first.ok_or(FetchError::Other("No sites on this Amber account".into()))?.id.clone();
                self.site = Some(id.clone());
                id
            }
        };
        let path = format!("/sites/{site}/prices/current");
        // Sent time, not arrival: that's the moment the next first check gets aimed at.
        let sent = Utc::now();
        let now: Vec<Interval> = get(&path, &[])?;
        let end = now.iter().find(|i| i.channel_type == "general").map(|i| i.end_time);
        let end = end.ok_or(FetchError::Other("Amber sent no general price".into()))?;
        let latest = end > Utc::now();
        if !forced && self.shown_end == Some(end) {
            self.checks_this_bracket += 1;
            self.last_miss = Some(sent);
            return Ok(latest);
        }
        let duration = now.iter().find(|i| i.channel_type == "general").map_or(0, |i| i.duration);
        self.learn_delay(end - TimeDelta::minutes(duration), sent);
        let chart: Vec<Interval> = get(
            &path,
            &[("next", "48".into()), ("previous", "12".into()), ("resolution", "30".into())],
        )?;
        let prices = build_prices(&now, &chart).map_err(FetchError::Other)?;
        if let Some((start, message)) = &prices.spike
            && self.alerted.insert(*start)
        {
            let settings = *self.settings.lock().unwrap();
            let _ = send_alert(message, &settings);
        }
        self.check_alerts(&prices);
        let _ = self.tray.send(tray_status(&prices, self.settings.lock().unwrap().tray));
        (self.send)(Update::Prices(Box::new(prices)));
        self.shown_end = Some(end);
        self.checks_this_bracket = 0;
        self.last_miss = None;

        if self.usage_at.is_none_or(|at| at.elapsed() >= USAGE_PERIOD) {
            self.refresh_usage(&site)?;
            self.usage_at = Some(Instant::now());
            (self.send)(Update::Usage(self.usage_days.clone()));
        }
        Ok(latest)
    }

    /// The first load goes back to the start of last month for the totals. After that only the last
    /// week is refetched, because a 5-minute site returns thousands of rows per week.
    fn refresh_usage(&mut self, site: &str) -> Result<(), FetchError> {
        let today = Local::now().date_naive();
        let start = if self.usage_days.is_empty() { start_of_previous_month(today) } else { today - TimeDelta::days(6) };
        // Only kept once every chunk has loaded, so a failure part way through can't leave a gap.
        let mut loaded = BTreeMap::new();
        let mut from = start;
        while from <= today {
            let to = (from + TimeDelta::days(6)).min(today);
            let rows: Vec<UsageRow> = get(
                &format!("/sites/{site}/usage"),
                &[("startDate", from.to_string()), ("endDate", to.to_string())],
            )?;
            loaded.extend(usage_by_day(&rows));
            from = to + TimeDelta::days(1);
        }
        self.usage_days.extend(loaded);
        Ok(())
    }

    fn learn_delay(&mut self, bracket_start: DateTime<Utc>, sent: DateTime<Utc>) {
        if self.on_schedule && self.shown_end == Some(bracket_start) {
            let seconds = |at: DateTime<Utc>| (at - bracket_start).num_milliseconds() as f64 / 1000.0;
            add_delay(&mut self.delays, delay_sample(seconds(sent), self.last_miss.map(seconds)));
            // Only a scheduling hint, so losing it to a failed write just means relearning.
            if let Ok(text) = serde_json::to_string(&self.delays) {
                let _ = fs::write(delays_path(), text);
            }
        }
        (self.send)(Update::Timing(timing(&self.delays, self.checks_this_bracket)));
    }

    fn check_alerts(&mut self, prices: &Prices) {
        let settings = *self.settings.lock().unwrap();
        let (fire_buy, buy_above) = crossed(Some(prices.buy), settings.buy_limit(), self.buy_above);
        let (fire_sell, sell_above) = crossed(prices.sell, settings.sell_limit(), self.sell_above);
        (self.buy_above, self.sell_above) = (buy_above, sell_above);
        // Below is above with both sides negated.
        let (fire_below, buy_below) = crossed(Some(-prices.buy), settings.buy_below_limit().map(|l| -l), self.buy_below);
        let best = prices.best_sell;
        let (fire_forecast, forecast_above) = crossed(best.map(|b| b.1), settings.sell_forecast_limit(), self.sell_forecast_above);
        (self.buy_below, self.sell_forecast_above) = (buy_below, forecast_above);
        let notify = |message: String| {
            let _ = send_alert(&message, &settings);
        };
        if fire_buy {
            notify(format!("Buy price is {:.1}c, above your {:.1}c alert", prices.buy, settings.buy));
        }
        if fire_sell {
            notify(format!("Sell price is {:.1}c, above your {:.1}c alert", prices.sell.unwrap_or_default(), settings.sell));
        }
        if fire_below {
            notify(format!("Buy price is {:.1}c, below your {:.1}c alert", prices.buy, settings.buy_below));
        }
        if let (true, Some((start, value))) = (fire_forecast, best) {
            let at = clock(start.with_timezone(&Local));
            notify(format!("Sell forecast to reach {value:.1}c at {at}, above your {:.1}c alert", settings.sell_forecast));
        }
    }

    fn run(mut self, wake: Receiver<()>) {
        (self.send)(Update::Timing(timing(&self.delays, 0)));
        let mut forced = true;
        let mut deadline = Utc::now();
        let mut failed = false;
        loop {
            let now = Utc::now();
            // More than a few seconds late means the PC slept, so this check can't time Amber.
            self.on_schedule = !forced && !failed && now - deadline < TimeDelta::seconds(5);
            failed = false;
            let next_first_check = |delays: &[f64]| next_bracket(Utc::now()) + TimeDelta::milliseconds(first_poll_ms(delays));
            deadline = if read_token().is_empty() {
                (self.send)(Update::NeedToken(""));
                next_first_check(&self.delays)
            } else {
                match self.cycle(forced) {
                    Ok(true) => {
                        (self.send)(Update::Done(None));
                        next_first_check(&self.delays)
                    }
                    Ok(false) => {
                        (self.send)(Update::Waiting);
                        let into_bracket = now.timestamp_millis().rem_euclid(BRACKET_SECONDS * 1000);
                        now + TimeDelta::milliseconds(poll_gap_ms(&self.delays, into_bracket))
                    }
                    Err(FetchError::Unauthorized) => {
                        self.site = None;
                        (self.send)(Update::NeedToken("Amber rejected that token. "));
                        next_first_check(&self.delays)
                    }
                    // Network, bad JSON, unexpected shape: the window keeps the last data on screen.
                    Err(FetchError::Other(e)) => {
                        (self.send)(Update::Done(Some(e)));
                        failed = true;
                        now + TimeDelta::seconds(ERROR_RETRY_SECONDS)
                    }
                }
            };
            match wait_until(&wake, deadline) {
                Some(woken) => forced = woken,
                None => return,
            }
        }
    }
}

fn start_of_previous_month(today: NaiveDate) -> NaiveDate {
    let (year, month) = if today.month() == 1 { (today.year() - 1, 12) } else { (today.year(), today.month() - 1) };
    NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(today)
}

/// Some(true) if Refresh woke us, Some(false) at the deadline, None once the window has closed.
/// Sleeps in short steps and rechecks the wall clock, so a suspended PC catches up when it wakes.
fn wait_until(wake: &Receiver<()>, deadline: DateTime<Utc>) -> Option<bool> {
    loop {
        // A negative remainder fails to_std: the deadline has already passed.
        let Ok(remaining) = (deadline - Utc::now()).to_std() else { return Some(false) };
        match wake.recv_timeout(remaining.min(Duration::from_secs(30))) {
            Ok(()) => return Some(true),
            Err(RecvTimeoutError::Timeout) if remaining <= Duration::from_secs(30) => return Some(false),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

// --- Window ---

#[derive(PartialEq)]
enum Tab {
    Settings,
    History,
}

struct App {
    updates: Receiver<Update>,
    wake: Sender<()>,
    prices: Option<Prices>,
    usage: Option<BTreeMap<String, Day>>,
    updated_at: Option<DateTime<Local>>,
    status: String,
    token_prompt: Option<&'static str>,
    token_input: String,
    settings: Arc<Mutex<Settings>>,
    timing: Option<Timing>,
    tab: Tab,
    tray_updates: Sender<tray::Status>,
    tray: tray::Pump,
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let (update_tx, updates) = mpsc::channel();
        let (wake, wake_rx) = mpsc::channel();
        let settings = Arc::new(Mutex::new(read_settings()));
        let (tray_tx, tray_rx) = mpsc::channel();
        let pump = tray::start(ctx.clone(), tray_rx);
        let worker = Worker {
            site: None,
            usage_at: None,
            alerted: HashSet::new(),
            settings: settings.clone(),
            buy_above: false,
            sell_above: false,
            buy_below: false,
            sell_forecast_above: false,
            tray: tray_tx.clone(),
            usage_days: BTreeMap::new(),
            shown_end: None,
            delays: read_delays(),
            checks_this_bracket: 0,
            last_miss: None,
            on_schedule: false,
            send: Box::new(move |update| {
                let _ = update_tx.send(update);
                ctx.request_repaint();
            }),
        };
        std::thread::spawn(move || worker.run(wake_rx));
        Self {
            updates,
            wake,
            prices: None,
            usage: None,
            updated_at: None,
            status: "Loading…".into(),
            token_prompt: None,
            token_input: String::new(),
            settings,
            timing: None,
            tab: Tab::Settings,
            tray_updates: tray_tx,
            tray: pump,
        }
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Prices(prices) => {
                self.prices = Some(*prices);
                self.updated_at = Some(Local::now());
            }
            Update::Usage(days) => self.usage = Some(days),
            Update::Timing(timing) => self.timing = Some(timing),
            Update::Waiting => self.status = "Waiting for Amber's latest 5-minute price…".into(),
            Update::NeedToken(problem) => {
                self.token_prompt = Some(problem);
                self.status = "An Amber API token is needed.".into();
            }
            Update::Done(None) => {
                self.token_prompt = None;
                self.status = format!("Updated {}", self.updated_at.map(clock).unwrap_or_default());
            }
            Update::Done(Some(error)) => {
                let stale = match self.updated_at {
                    Some(at) => format!("Still showing {} data.", clock(at)),
                    None => "No data yet.".into(),
                };
                self.status = format!("Update failed: {error}. {stale} Retrying in {ERROR_RETRY_SECONDS}s.");
            }
        }
    }

    fn token_form(&mut self, ui: &mut egui::Ui, problem: &str) {
        ui.label(format!("{problem}Generate a token at:"));
        ui.hyperlink("https://app.amber.com.au/developers");
        ui.label(format!("Paste it here. It's stored only in {}", token_path().display()));
        let field = ui.add(egui::TextEdit::singleline(&mut self.token_input).password(true).desired_width(f32::INFINITY));
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("Save").clicked() || entered) && !self.token_input.trim().is_empty() {
            self.status = match save_token(self.token_input.trim()) {
                Ok(()) => {
                    self.token_input.clear();
                    self.token_prompt = None;
                    let _ = self.wake.send(());
                    "Loading…".into()
                }
                Err(e) => format!("Couldn't save the token: {e}"),
            };
        }
        ui.separator();
    }

    fn now_section(&mut self, ui: &mut egui::Ui) {
        if let Some(problem) = self.token_prompt {
            self.token_form(ui, problem);
        }
        let prices = self.prices.as_ref();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 28.0;
            let dash = || "–".to_string();
            tile(ui, "Buy from grid", prices.map_or_else(dash, |p| format!("{:.1}c", p.buy)), prices.map_or("", |p| &p.buy_note));
            let sell = prices.and_then(|p| p.sell);
            let note = match (prices, sell) {
                (None, _) => "",
                (Some(_), None) => "no feed-in channel",
                (_, Some(s)) if s < 0.0 => "you pay to export",
                _ => "you earn",
            };
            tile(ui, "Sell to grid", sell.map_or_else(dash, |s| format!("{s:.1}c")), note);
            tile(ui, "Grid renewables", prices.map_or_else(dash, |p| format!("{:.0}%", p.renewables)), if prices.is_some() { "right now" } else { "" });
        });
        ui.add_space(6.0);
        if let Some(p) = prices {
            ui.label(RichText::new(p.advice).strong());
            if let Some((_, message)) = &p.spike {
                ui.label(RichText::new(message).strong());
            }
            if !p.tariff.is_empty() {
                ui.label(&p.tariff);
            }
            if !p.export.is_empty() {
                ui.label(&p.export);
            }
            ui.weak(&p.windows);
            draw_chart(ui, &p.chart_buy, &p.chart_sell);
            ui.weak("Solid: actual. Dashed: forecast, with its likely range shaded. Grey: peak. Top strip: export charge or bonus.");
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Alerts").strong());
        let saved = *self.settings.lock().unwrap();
        let mut edited = saved;
        egui::Grid::new("alerts").spacing(vec2(12.0, 4.0)).show(ui, |ui| {
            alert_row(ui, "Buy goes above", &mut edited.buy_on, &mut edited.buy);
            alert_row(ui, "Buy falls below", &mut edited.buy_below_on, &mut edited.buy_below);
            alert_row(ui, "Sell goes above", &mut edited.sell_on, &mut edited.sell);
            alert_row(ui, "Sell forecast above", &mut edited.sell_forecast_on, &mut edited.sell_forecast);
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut edited.sound_on, "Play");
            ui.add_enabled_ui(edited.sound_on, |ui| {
                egui::ComboBox::from_id_salt("sound").selected_text(edited.sound.label()).show_ui(ui, |ui| {
                    for sound in Sound::ALL {
                        ui.selectable_value(&mut edited.sound, sound, sound.label());
                    }
                });
            });
            if ui.button("Send test alert").clicked() {
                self.status = match send_alert("This is a test alert.", &edited) {
                    Ok(()) => "Test alert sent.".into(),
                    Err(e) => format!("Couldn't send the test alert: {e}"),
                };
            }
        });
        if let Some(p) = &self.prices {
            if edited.buy_limit().is_some_and(|limit| p.buy > limit) {
                ui.label(RichText::new(format!("Buy {:.1}c is above your {:.1}c alert", p.buy, edited.buy)).strong());
            }
            if edited.buy_below_limit().is_some_and(|limit| p.buy < limit) {
                ui.label(RichText::new(format!("Buy {:.1}c is below your {:.1}c alert", p.buy, edited.buy_below)).strong());
            }
            if let (Some((start, value)), Some(limit)) = (p.best_sell, edited.sell_forecast_limit())
                && value > limit
            {
                let at = clock(start.with_timezone(&Local));
                ui.label(RichText::new(format!("Sell forecast {value:.1}c at {at} is above your {limit:.1}c alert")).strong());
            }
            if let (Some(sell), Some(limit)) = (p.sell, edited.sell_limit())
                && sell > limit
            {
                ui.label(RichText::new(format!("Sell {sell:.1}c is above your {limit:.1}c alert")).strong());
            }
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Tray icon shows").strong());
        ui.horizontal(|ui| {
            ui.radio_value(&mut edited.tray, tray::Show::Buy, "Buy price");
            ui.radio_value(&mut edited.tray, tray::Show::Sell, "Sell price");
            ui.radio_value(&mut edited.tray, tray::Show::Logo, "Logo only");
        });
        if edited != saved {
            *self.settings.lock().unwrap() = edited;
            if let Err(e) = save_settings(&edited) {
                self.status = format!("Couldn't save settings: {e}");
            }
            if let Some(p) = &self.prices {
                let _ = self.tray_updates.send(tray_status(p, edited.tray));
            }
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Price check timing").strong());
        match &self.timing {
            Some(timing) => timing_view(ui, timing),
            None => {
                ui.weak("Nothing measured yet");
            }
        }
    }

    fn history_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Grid meter").strong());
        match &self.usage {
            Some(days) if !days.is_empty() => {
                let label = |month: &str| {
                    NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d")
                        .map_or(month.to_string(), |d| d.format("%B %Y").to_string())
                };
                let months = month_totals(days).into_iter().take(2).map(|(m, d)| (label(&m), d)).collect();
                usage_grid(ui, "months", months);
                ui.add_space(4.0);
                let recent = days.iter().rev().take(7).map(|(day, totals)| {
                    let name = NaiveDate::parse_from_str(day, "%Y-%m-%d")
                        .map_or(day.clone(), |d| d.format("%a %-d %b").to_string());
                    (name, *totals)
                });
                usage_grid(ui, "days", recent.collect());
            }
            _ => {
                ui.weak("No meter data yet");
            }
        }
        ui.weak("Energy charges only, so bills will also include daily supply and Amber's fee. Amber doesn't report solar generation or battery level.");
    }
}

fn timing_view(ui: &mut egui::Ui, timing: &Timing) {
    egui::Grid::new("timing").spacing(vec2(12.0, 2.0)).show(ui, |ui| {
        let mut row = |name: &str, value: String| {
            ui.weak(name);
            ui.label(value);
            ui.end_row();
        };
        if timing.stored >= MIN_DELAY_SAMPLES {
            row("Amber usually publishes", format!("{:.3}s into each bracket", percentile(&timing.recent, 0.5)));
        } else {
            row("Still learning", format!("{} of {MIN_DELAY_SAMPLES} brackets measured", timing.stored));
        }
        row("First check", format!("{:.3}s into each bracket", timing.first_check));
        row("Then checks", format!("every {:.1}s until {:.3}s, then every {:.0}s", FAST_POLL_MS as f64 / 1000.0, timing.fast_until, SLOW_POLL_MS as f64 / 1000.0));
        row("Missed checks last bracket", timing.checks.to_string());
        let recent: Vec<String> = timing.recent.iter().map(|s| format!("{s:.3}s")).collect();
        row("Recent delays, newest first", recent.join(", "));
        row("Delays stored", format!("{} of {MAX_DELAY_SAMPLES}, only the last {RECENT_DELAY_SAMPLES} steer the checks", timing.stored));
    });
}

fn alert_row(ui: &mut egui::Ui, name: &str, on: &mut bool, limit: &mut f64) {
    ui.checkbox(on, name);
    ui.add_enabled(*on, egui::DragValue::new(limit).speed(0.5).max_decimals(1).suffix("c"));
    ui.end_row();
}

fn tile(ui: &mut egui::Ui, title: &str, value: String, note: &str) {
    ui.vertical(|ui| {
        ui.weak(title);
        ui.label(RichText::new(value).size(30.0).strong());
        ui.weak(note);
    });
}

fn usage_grid(ui: &mut egui::Ui, id: &str, rows: Vec<(String, Day)>) {
    egui::Grid::new(id).spacing(vec2(20.0, 2.0)).min_col_width(90.0).show(ui, |ui| {
        for heading in ["", "Bought", "Sold", "Net cost"] {
            ui.weak(heading);
        }
        ui.end_row();
        for (name, totals) in rows {
            ui.label(name);
            ui.label(format!("{:.1} kWh", totals.bought));
            ui.label(format!("{:.1} kWh", totals.sold));
            ui.label(dollars(totals.net_dollars));
            ui.end_row();
        }
    });
}

fn draw_chart(ui: &mut egui::Ui, buy: &Series, sell: &Series) {
    let (left, right, top, bottom) = (40.0, 10.0, 24.0, 20.0);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 190.0), Sense::hover());
    let (Some(first), Some(last)) = (buy.first(), buy.last()) else { return };
    let painter = ui.painter_at(rect);
    let ink = ui.visuals().text_color();
    let font = FontId::proportional(11.0);
    let plot_w = rect.width() - left - right;
    let plot_h = rect.height() - top - bottom;
    let (t0, t1) = (first.start.timestamp() as f32, last.end.timestamp() as f32);
    // Scale to the lines, not the ranges: one wide spike range would flatten everything else.
    let values = || buy.iter().chain(sell).map(|p| p.value);
    let low = values().fold(0.0, f64::min);
    let high = values().fold(0.0, f64::max);
    let high = if high > low { high } else { low + 1.0 };
    let step = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000]
        .into_iter()
        .find(|s| (high - low) / *s as f64 <= 4.0)
        .unwrap_or(50000);
    let low = (low / step as f64).floor() * step as f64;
    let high = (high / step as f64).ceil() * step as f64;
    let x_at = |moment: DateTime<Local>| rect.left() + left + (moment.timestamp() as f32 - t0) / (t1 - t0) * plot_w;
    let y_at = |value: f64| rect.top() + top + ((high - value) / (high - low)) as f32 * plot_h;
    let (plot_top, plot_bottom) = (rect.top() + top, rect.top() + top + plot_h);
    let plot = egui::Rect::from_min_max(pos2(rect.left() + left, plot_top), pos2(rect.right() - right, plot_bottom));

    for p in buy.iter().filter(|p| p.peak) {
        let band = egui::Rect::from_x_y_ranges(x_at(p.start)..=x_at(p.end), plot_top..=plot_bottom);
        painter.rect_filled(band, 0.0, ink.gamma_multiply(0.07));
    }

    let mut tick = (low / step as f64).floor() as i64 * step;
    while tick as f64 <= high {
        let y = y_at(tick as f64);
        if y >= plot_top - 1.0 {
            let alpha = if tick == 0 { 0.55 } else { 0.13 };
            painter.hline(rect.left() + left..=rect.right() - right, y, Stroke::new(1.0, ink.gamma_multiply(alpha)));
            painter.text(pos2(rect.left() + left - 6.0, y), Align2::RIGHT_CENTER, format!("{tick}c"), font.clone(), ink.gamma_multiply(0.75));
        }
        tick += step;
    }

    let mut moment = first.start - TimeDelta::seconds(first.start.timestamp().rem_euclid(3600));
    while (moment.timestamp() as f32) < t1 {
        if moment >= first.start && moment.hour() % 6 == 0 {
            let label = moment.format("%-I%P").to_string();
            painter.text(pos2(x_at(moment), rect.bottom() - 2.0), Align2::CENTER_BOTTOM, label, font.clone(), ink.gamma_multiply(0.75));
        }
        moment += TimeDelta::hours(1);
    }

    let now = Local::now();
    if first.start < now && now < last.end {
        let x = x_at(now);
        let dash = Stroke::new(1.0, ink.gamma_multiply(0.6));
        painter.extend(Shape::dashed_line(&[pos2(x, plot_top), pos2(x, plot_bottom)], dash, 3.0, 3.0));
    }

    for (series, color) in [(buy, BUY_COLOR), (sell, SELL_COLOR)] {
        for p in series {
            if let Some((a, b)) = p.range {
                let band = egui::Rect::from_x_y_ranges(x_at(p.start)..=x_at(p.end), y_at(b)..=y_at(a));
                painter.with_clip_rect(plot).rect_filled(band, 0.0, color.gamma_multiply(0.18));
            }
        }
        let steps = |rows: &[&Point]| -> Vec<_> {
            rows.iter().flat_map(|p| [pos2(x_at(p.start), y_at(p.value)), pos2(x_at(p.end), y_at(p.value))]).collect()
        };
        let actual: Vec<&Point> = series.iter().filter(|p| !p.forecast).collect();
        // The forecast line starts where the actual one stops, so the two join up.
        let forecast: Vec<&Point> = series.iter().filter(|p| p.forecast).collect();
        let stroke = Stroke::new(2.0, color);
        painter.add(Shape::line(steps(&actual), stroke));
        let mut ahead = steps(&forecast);
        if let (Some(a), Some(b)) = (actual.last(), forecast.first()) {
            ahead.insert(0, pos2(x_at(b.start), y_at(a.value)));
        }
        painter.extend(Shape::dashed_line(&ahead, stroke, 5.0, 3.0));
    }

    // Strip along the top marks when the sell price carries an export charge or bonus.
    for p in sell.iter().filter(|p| p.export_adjustment.is_some()) {
        let strip = egui::Rect::from_x_y_ranges(x_at(p.start)..=x_at(p.end), plot_top..=plot_top + 3.0);
        painter.rect_filled(strip, 0.0, SELL_COLOR);
    }

    // Legend doubles as the readout: the hovered time, otherwise now.
    let mut at = now;
    if let Some(pointer) = response.hover_pos() {
        let seconds = (pointer.x - rect.left() - left) / plot_w * (t1 - t0);
        at = first.start + TimeDelta::seconds(seconds as i64);
        painter.vline(pointer.x, plot_top..=plot_bottom, Stroke::new(1.0, ink.gamma_multiply(0.6)));
    }
    let mut x = rect.left() + left;
    for (name, series, color) in [("Buy", buy, BUY_COLOR), ("Sell", sell, SELL_COLOR)] {
        if series.is_empty() {
            continue;
        }
        let point = series.iter().find(|p| p.start <= at && at < p.end);
        let label = point.map_or(name.to_string(), |p| match p.range {
            Some((a, b)) => format!("{name} {:.1}c ({a:.0}–{b:.0})", p.value),
            None => format!("{name} {:.1}c", p.value),
        });
        painter.rect_filled(egui::Rect::from_min_size(pos2(x, rect.top() + 6.0), vec2(10.0, 10.0)), 0.0, color);
        let drawn = painter.text(pos2(x + 14.0, rect.top() + 11.0), Align2::LEFT_CENTER, label, font.clone(), ink);
        x = drawn.right() + 12.0;
    }
    painter.text(pos2(x, rect.top() + 11.0), Align2::LEFT_CENTER, clock(at), font, ink.gamma_multiply(0.75));
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(update) = self.updates.try_recv() {
            self.apply(update);
        }
        self.tray.drain();
        // Keeps the chart's "now" marker moving.
        ui.ctx().request_repaint_after(Duration::from_secs(60));

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Refresh").clicked() {
                    let _ = self.wake.send(());
                }
                ui.weak(&self.status);
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let half = ui.available_height() / 2.0;
            egui::ScrollArea::vertical().id_salt("now").max_height(half).auto_shrink([false, false]).show(ui, |ui| {
                self.now_section(ui);
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.selectable_value(&mut self.tab, Tab::History, "History");
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("tab").auto_shrink([false, false]).show(ui, |ui| match self.tab {
                Tab::Settings => self.settings_tab(ui),
                Tab::History => self.history_tab(ui),
            });
        });
    }
}

/// UI scale that makes egui's 12.5px body text match a desktop font like "'Ubuntu 14'" at 96 dpi.
fn font_zoom(font_name: &str, text_scaling: &str) -> Option<f32> {
    let points: f32 = font_name.trim().trim_matches('\'').rsplit(' ').next()?.parse().ok()?;
    let scaling: f32 = text_scaling.trim().parse().ok()?;
    Some((points * 96.0 / 72.0 * scaling / 12.5).clamp(0.75, 4.0))
}

/// Linux desktop text size. Other platforms and desktops have no gsettings, so they keep the default.
fn desktop_zoom() -> f32 {
    let cinnamon = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.contains("Cinnamon"));
    let schema = if cinnamon { "org.cinnamon.desktop.interface" } else { "org.gnome.desktop.interface" };
    let setting = |key: &str| {
        let output = std::process::Command::new("gsettings").args(["get", schema, key]).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    // ponytail: read once at startup, so a changed font size applies after restarting the app.
    setting("font-name").zip(setting("text-scaling-factor")).and_then(|(f, t)| font_zoom(&f, &t)).unwrap_or(1.0)
}

fn main() -> eframe::Result {
    let zoom = desktop_zoom();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Amber")
        .with_app_id("amber-widget")
        .with_inner_size([520.0 * zoom, 900.0 * zoom]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "Amber",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_zoom_factor(zoom);
            Ok(Box::new(App::new(cc.egui_ctx.clone())))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUYS: [f64; 4] = [10.0, 20.0, 30.0, 40.0];
    const SELLS: [f64; 4] = [2.0, 4.0, 6.0, 8.0];

    #[test]
    fn advice_picks_the_right_message() {
        assert!(advice(100.0, Some(50.0), &BUYS, &SELLS, "spike", false).contains("spike"));
        assert!(advice(20.0, Some(-1.0), &BUYS, &SELLS, "none", false).contains("costs you money"));
        assert!(advice(-2.0, Some(3.0), &BUYS, &SELLS, "none", false).contains("paid to use"));
        assert!(advice(25.0, Some(12.0), &BUYS, &SELLS, "none", false).contains("export"));
        assert!(advice(10.0, Some(3.0), &BUYS, &SELLS, "none", false).contains("Cheap"));
        assert!(advice(25.0, Some(3.0), &BUYS, &SELLS, "none", false).contains("Nothing unusual"));
        assert!(advice(25.0, None, &[], &[], "none", false).contains("Nothing unusual"));
        // A top-quartile sell price isn't worth taking if recharging later costs more.
        assert!(!advice(25.0, Some(9.0), &BUYS, &SELLS, "none", false).contains("export"));
    }

    #[test]
    fn usage_groups_by_day_and_channel() {
        let row = |channel: &str, kwh, cost| UsageRow { date: "2026-11-01".into(), channel_type: channel.into(), kwh, cost };
        let days = usage_by_day(&[row("general", 2.0, 50.0), row("controlledLoad", 1.0, 10.0), row("feedIn", -4.0, -20.0)]);
        let day = days["2026-11-01"];
        assert_eq!((day.bought, day.sold), (3.0, 4.0));
        assert!((day.net_dollars - 0.4).abs() < 1e-9);
    }

    #[test]
    fn parses_amber_interval_and_finds_spikes() {
        let json = r#"[
          {"type":"ActualInterval","duration":30,"perKwh":9,"spotPerKwh":5,"renewables":40,"channelType":"general","spikeStatus":"spike","descriptor":"extremelyLow",
           "startTime":"2026-11-01T00:00:00Z","endTime":"2026-11-01T00:30:00Z"},
          {"type":"ForecastInterval","duration":30,"perKwh":5,"spotPerKwh":2,"renewables":40,"channelType":"general","spikeStatus":"potential","descriptor":"low",
           "startTime":"2026-11-01T00:30:00Z","endTime":"2026-11-01T01:00:00Z","advancedPrice":{"low":1,"predicted":3,"high":9},"tariffInformation":{"period":"solarSponge","season":"nonSummer"}}
        ]"#;
        let rows: Vec<Interval> = serde_json::from_str(json).unwrap();
        assert_eq!(forecast_cents(&rows[0]), 9.0);
        assert_eq!(forecast_cents(&rows[1]), 3.0);
        assert_eq!(next_spike(&rows).unwrap().kind, "ForecastInterval");
        assert_eq!(describe(&rows[0].descriptor), "extremely low");
    }

    #[test]
    fn zoom_follows_desktop_font_and_scaling() {
        assert!((font_zoom("'Ubuntu 14'\n", "1.0\n").unwrap() - 1.4933).abs() < 0.001);
        assert!((font_zoom("'Noto Sans Bold 10'", "1.5").unwrap() - 1.6).abs() < 0.001);
        assert_eq!(font_zoom("'Ubuntu'", "1.0"), None);
    }

    #[test]
    fn next_bracket_lands_on_the_next_five_minutes() {
        let at = |text: &str| text.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(next_bracket(at("2026-11-02T05:03:20Z")), at("2026-11-02T05:05:00Z"));
        assert_eq!(next_bracket(at("2026-11-02T05:05:00Z")), at("2026-11-02T05:10:00Z"));
    }

    #[test]
    fn alerts_fire_once_per_crossing() {
        assert_eq!(crossed(Some(51.0), Some(50.0), false), (true, true));
        assert_eq!(crossed(Some(52.0), Some(50.0), true), (false, true));
        assert_eq!(crossed(Some(49.0), Some(50.0), true), (false, false));
        assert_eq!(crossed(Some(50.0), Some(50.0), false), (false, false));
        assert_eq!(crossed(Some(99.0), None, false), (false, false));
        assert_eq!(crossed(None, Some(50.0), false), (false, false));
    }

    #[test]
    fn alert_settings_survive_missing_fields() {
        let settings: Settings = serde_json::from_str(r#"{"buy_on":true,"buy":42.5}"#).unwrap();
        assert_eq!(settings.buy_limit(), Some(42.5));
        assert_eq!(settings.sell_limit(), None);
    }

    #[test]
    fn polling_follows_the_learned_delay() {
        // Too little history: check from the boundary, fast for the first 30 seconds.
        assert_eq!(first_poll_ms(&[25.0; 4]), 0);
        assert_eq!(poll_gap_ms(&[25.0; 4], 29_999), FAST_POLL_MS);
        assert_eq!(poll_gap_ms(&[25.0; 4], 30_000), SLOW_POLL_MS);
        // Learned 25.25s: sleep until just before it, slow down once well past it.
        assert_eq!(first_poll_ms(&[25.25; 5]), 24_250);
        assert_eq!(poll_gap_ms(&[25.25; 5], 30_249), FAST_POLL_MS);
        assert_eq!(poll_gap_ms(&[25.25; 5], 30_250), SLOW_POLL_MS);
        // Never earlier than the boundary.
        assert_eq!(first_poll_ms(&[0.5; 5]), 0);
    }

    #[test]
    fn delay_samples_use_what_the_checks_prove() {
        assert_eq!(delay_sample(25.0, Some(24.0)), 24.5);
        assert_eq!(delay_sample(25.0, None), 12.5);
        assert_eq!(delay_sample(1.0, Some(-3.0)), 0.0);
    }

    /// Plays `brackets` of checks against a fixed publish time, returning the first check it settles on.
    fn settle(delays: &mut Vec<f64>, publish: f64, brackets: usize) -> f64 {
        for _ in 0..brackets {
            let mut at = first_poll_ms(delays);
            let mut miss = None;
            while (at as f64) < publish * 1000.0 {
                miss = Some(at as f64 / 1000.0);
                at += poll_gap_ms(delays, at);
            }
            add_delay(delays, delay_sample(at as f64 / 1000.0, miss));
        }
        first_poll_ms(delays) as f64 / 1000.0
    }

    #[test]
    fn estimate_adapts_within_an_hour_either_way() {
        let mut delays = vec![40.0; 100];
        let first = settle(&mut delays, 10.0, 12);
        assert!((8.0..=10.0).contains(&first), "earlier: {first}");
        assert_eq!(delays.len(), MAX_DELAY_SAMPLES);
        let first = settle(&mut delays, 40.0, 12);
        assert!((37.0..=40.0).contains(&first), "later: {first}");
    }

    #[test]
    fn one_odd_bracket_does_not_move_the_first_check() {
        let mut delays = vec![25.0; 6];
        add_delay(&mut delays, 5.0);
        assert_eq!(first_poll_ms(&delays), 24_000);
    }

    #[test]
    fn timing_view_lists_newest_first() {
        let view = timing(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], 3);
        assert_eq!(view.recent, [7.0, 6.0, 5.0, 4.0, 3.0, 2.0]);
        assert_eq!((view.stored, view.checks), (7, 3));
    }

    #[test]
    fn old_settings_files_get_the_new_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"buy_on":true}"#).unwrap();
        assert!(settings.sound_on);
        assert!(settings.tray == tray::Show::Buy);
    }

    fn interval(json: &str) -> Interval {
        let base = r#""duration":30,"perKwh":10,"spotPerKwh":5,"renewables":40,"channelType":"general","spikeStatus":"none","descriptor":"low""#;
        serde_json::from_str(&format!("{{{base},{json}}}")).unwrap()
    }

    #[test]
    fn export_adjustments_match_real_tariff_96200_prices() {
        // Real rows: normal hours, non-summer 11am-1pm charge, summer 5-8pm bonus, and a low spot.
        assert_eq!(export_adjustment(0.9593 * 15.0, 15.0), None);
        assert!((export_adjustment(0.9593 * 15.0 - 2.21, 15.0).unwrap() + 2.21).abs() < 0.01);
        assert!((export_adjustment(27.46136, 15.91336).unwrap() - 12.19).abs() < 0.01);
        assert_eq!(export_adjustment(0.9593 * -0.9, -0.9), None);
        // Small errors in the loss factor must not look like a charge during a big spike.
        assert_eq!(export_adjustment(0.96 * 1000.0, 1000.0), None);
    }

    #[test]
    fn advice_steers_away_from_exporting_under_a_charge() {
        assert!(advice(25.0, Some(12.0), &BUYS, &SELLS, "none", true).contains("Export charge"));
        // A negative sell price or spike still matters more.
        assert!(advice(25.0, Some(-1.0), &BUYS, &SELLS, "none", true).contains("costs you money"));
    }

    #[test]
    fn tariff_line_names_the_period_and_when_it_ends() {
        let time = |t: &str| format!(r#""startTime":"2026-11-02T{t}:00:00Z","endTime":"2026-11-02T{t}:30:00Z""#);
        let peak = r#""tariffInformation":{"period":"peak","season":"summer"}"#;
        let shoulder = r#""tariffInformation":{"period":"shoulder","season":"summer"}"#;
        let current = interval(&format!(r#""type":"CurrentInterval",{},{peak}"#, time("08")));
        let next = interval(&format!(r#""type":"ForecastInterval",{},{peak}"#, time("09")));
        let after = interval(&format!(r#""type":"ForecastInterval",{},{shoulder}"#, time("10")));
        let end = clock("2026-11-02T10:00:00Z".parse::<DateTime<Utc>>().unwrap().with_timezone(&Local));
        assert_eq!(tariff_line(&current, &[next, after]), format!("Peak until {end} (summer rates)"));
    }

    #[test]
    fn export_line_reports_current_then_upcoming() {
        let at = |t: &str| clock(format!("2026-11-02T{t}:00:00Z").parse::<DateTime<Utc>>().unwrap().with_timezone(&Local));
        let rows: Vec<Interval> = serde_json::from_str(&format!(
            r#"[{}]"#,
            [("08", -9.6, 10.0), ("09", -21.6, 10.0), ("10", -9.6, 10.0)]
                .map(|(t, per, spot)| format!(r#"{{"type":"ForecastInterval","duration":30,"perKwh":{per},"spotPerKwh":{spot},"renewables":40,"channelType":"feedIn","spikeStatus":"none","descriptor":"low","startTime":"2026-11-02T{t}:00:00Z","endTime":"2026-11-02T{t}:30:00Z"}}"#))
                .join(",")
        ))
        .unwrap();
        assert_eq!(export_line(None, &rows), format!("12.0c export bonus from {}", at("09")));
        assert_eq!(export_line(Some(-2.2), &rows), format!("Sell includes a 2.2c export charge until {}", at("08")));
        assert_eq!(export_line(None, &rows[..1]), "");
    }

    #[test]
    fn months_total_their_days_newest_first() {
        let day = |bought, sold, net_dollars| Day { bought, sold, net_dollars };
        let days = BTreeMap::from([
            ("2026-10-30".to_string(), day(1.0, 2.0, 0.5)),
            ("2026-10-31".to_string(), day(1.0, 2.0, -1.0)),
            ("2026-11-01".to_string(), day(3.0, 4.0, 2.0)),
        ]);
        let months = month_totals(&days);
        assert_eq!(months[0], ("2026-11".to_string(), day(3.0, 4.0, 2.0)));
        assert_eq!(months[1], ("2026-10".to_string(), day(2.0, 4.0, -0.5)));
        let start = |y, m, d| start_of_previous_month(NaiveDate::from_ymd_opt(y, m, d).unwrap()).to_string();
        assert_eq!(start(2026, 11, 2), "2026-10-01");
        assert_eq!(start(2027, 1, 15), "2026-12-01");
    }

    #[test]
    fn formats_money_and_percentiles() {
        assert_eq!(dollars(-1.5), "-$1.50");
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.25), 2.0);
        assert_eq!(earn_cents(-7.5), 7.5);
    }
}
