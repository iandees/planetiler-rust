//! Port of `config/Arguments.java`: lightweight abstraction over ways to provide key/value pair arguments to a program
//! like command-line arguments, environment variables, or a config file.
//!
//! When looking up a key, tries to find a case-and-separator-insensitive match, for example `"CONFIG_OPTION"` will
//! match `"config-option"` and `"config_option"`.
//!
//! If you replace an option with a new value, you can read a value from the new option and fall back to the old one by
//! using `"new_flag|old_flag"` as the key.
//!
//! Each getter takes a description of the option, which is logged at debug level along with the value that was used.
//! Getters that need to parse their value return `anyhow::Result` (Java throws `NumberFormatException` /
//! `IllegalArgumentException`).
//!
//! Java also reads JVM system properties (`-Dplanetiler.key=value`); there's no equivalent in Rust, so
//! [`Arguments::from_env_or_args`] only reads command-line arguments and environment variables.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::geo::envelope::Envelope;
use crate::geo::geo_utils;
use crate::stats::stats::StatsRef;

type Provider = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
type Keys = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Source of configuration options. Cheap to clone.
#[derive(Clone)]
pub struct Arguments {
    provider: Provider,
    keys: Keys,
    silent: Arc<AtomicBool>,
    log_once: Option<Arc<Mutex<HashMap<String, usize>>>>,
}

impl Default for Arguments {
    fn default() -> Self {
        Arguments::of()
    }
}

impl fmt::Debug for Arguments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Arguments")
    }
}

fn normalize_with(key: &str, separator: &str, upper_case: bool) -> String {
    let mut result = String::with_capacity(key.len());
    for c in key.chars() {
        if matches!(c, '.' | '_' | '-') {
            result.push_str(separator);
        } else {
            result.push(c);
        }
    }
    if upper_case { result.to_uppercase() } else { result.to_lowercase() }
}

fn normalize(key: &str) -> String {
    normalize_with(key, "_", false)
}

impl Arguments {
    fn new(provider: Provider, keys: Keys) -> Arguments {
        Arguments { provider, keys, silent: Arc::new(AtomicBool::new(false)), log_once: None }
    }

    /// Empty arguments.
    pub fn of() -> Arguments {
        Arguments::of_map(Vec::<(String, String)>::new())
    }

    /// Arguments from key/value pairs (Java's `Arguments.of(Object...)`).
    pub fn of_pairs<K: ToString, V: ToString>(pairs: impl IntoIterator<Item = (K, V)>) -> Arguments {
        Arguments::of_map(pairs.into_iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    /// Arguments from a map of keys to values (Java's `Arguments.of(Map)`).
    pub fn of_map<K: AsRef<str>, V: Into<String>>(map: impl IntoIterator<Item = (K, V)>) -> Arguments {
        let updated: Arc<IndexMap<String, String>> =
            Arc::new(map.into_iter().map(|(k, v)| (normalize(k.as_ref()), v.into())).collect());
        let keys = updated.clone();
        Arguments::new(Arc::new(move |key| updated.get(key).cloned()), Arc::new(move || keys.keys().cloned().collect()))
    }

    /// Returns arguments parsed from a properties-style map (Java's `Arguments.from(Properties)`); keys are matched
    /// exactly by the provider after normalization of the requested key.
    pub fn from_properties(properties: IndexMap<String, String>) -> Arguments {
        let properties = Arc::new(properties);
        let keys = properties.clone();
        Arguments::new(
            Arc::new(move |key| properties.get(key).cloned()),
            Arc::new(move || keys.keys().cloned().collect()),
        )
    }

    /// Returns arguments parsed from command-line arguments.
    ///
    /// For example to set `key=value`: `planetiler key=value` or `planetiler --key value`, or to set `key=true`:
    /// `planetiler --key`.
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Arguments {
        let mut parsed: IndexMap<String, String> = IndexMap::new();
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_ref().trim();
            let (raw_key, value) = match arg.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (arg, None),
            };
            let key = raw_key.trim_start_matches(|c: char| c.is_whitespace() || c == '-').to_string();
            if let Some(value) = value {
                parsed.insert(key, value.to_string());
            } else if arg.starts_with('-') {
                if i >= args.len() - 1 || args[i + 1].as_ref().trim().starts_with('-') {
                    parsed.insert(key, "true".to_string());
                } else {
                    i += 1;
                    parsed.insert(key, args[i].as_ref().trim().to_string());
                }
            } else {
                parsed.insert(key, "true".to_string());
            }
            i += 1;
        }
        Arguments::of_map(parsed)
    }

    /// Returns arguments provided from a `.properties` file.
    pub fn from_config_file(path: &Path) -> anyhow::Result<Arguments> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("Unable to load config file: {}", path.display()))?;
        Ok(Arguments::from_properties(parse_properties(&contents)))
    }

    /// Returns arguments parsed from environment variables prefixed with `PLANETILER_`.
    ///
    /// For example to set `key=value`: `PLANETILER_KEY=value planetiler ...`
    pub fn from_environment() -> Arguments {
        Arguments::from_environment_with(|key| std::env::var(key).ok(), || std::env::vars().map(|(k, _)| k).collect())
    }

    /// Returns arguments parsed from environment variables with a custom getter and key listing (for tests).
    pub fn from_environment_with(
        getter: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        keys: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Arguments {
        Self::from_prefixed(Arc::new(getter), Arc::new(keys), "PLANETILER", "_", true)
    }

    /// Returns arguments from JVM-style properties prefixed with `planetiler.`, with a custom getter and key listing.
    ///
    /// Rust has no JVM system properties; this exists so other property sources can be adapted the same way.
    pub fn from_jvm_properties_with(
        getter: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        keys: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Arguments {
        Self::from_prefixed(Arc::new(getter), Arc::new(keys), "planetiler", ".", false)
    }

    /// Returns arguments parsed from command-line arguments or environment variables, in that priority order.
    pub fn from_env_or_args<S: AsRef<str>>(args: &[S]) -> Arguments {
        Arguments::from_args(args).or_else(Arguments::from_environment())
    }

    /// Returns arguments parsed from command-line arguments, environment variables, or a config file from the
    /// `config` argument, in that priority order.
    pub fn from_args_or_config_file<S: AsRef<str>>(args: &[S]) -> anyhow::Result<Arguments> {
        let from_args_or_env = Arguments::from_env_or_args(args);
        match from_args_or_env.file_opt("config", "path to config file") {
            Some(config_file) => Ok(from_args_or_env.or_else(Arguments::from_config_file(&config_file)?)),
            None => Ok(from_args_or_env),
        }
    }

    fn from_prefixed(provider: Provider, raw_keys: Keys, prefix: &str, separator: &str, upper_case: bool) -> Arguments {
        let normalized_prefix = normalize_with(&format!("{prefix}{separator}"), separator, upper_case);
        let prefix = prefix.to_string();
        let separator = separator.to_string();
        let prefix_lower = normalized_prefix.to_lowercase();
        let reverse = move |key: &str| -> String {
            let stripped = if key.to_lowercase().starts_with(&prefix_lower) { &key[prefix_lower.len()..] } else { key };
            normalize(stripped)
        };
        let keys: Keys = Arc::new(move || {
            raw_keys()
                .into_iter()
                .filter_map(|key| {
                    let reversed = reverse(&key);
                    if normalize(&key) == normalize(&reversed) { None } else { Some(reversed) }
                })
                .collect()
        });
        let forward = move |key: &str| normalize_with(&format!("{prefix}{separator}{key}"), &separator, upper_case);
        Arguments::new(Arc::new(move |key| provider(&forward(key))), keys)
    }

    fn get(&self, key: &str) -> Option<String> {
        for (i, option) in key.split('|').enumerate() {
            let option = option.trim();
            if let Some(value) = (self.provider)(&normalize(option)) {
                if i != 0 {
                    tracing::warn!("Argument '{option}' is deprecated");
                }
                return Some(value);
            }
        }
        None
    }

    /// Chain two argument providers so that `other` is used as a fallback to `self`.
    pub fn or_else(&self, other: Arguments) -> Arguments {
        let us = self.clone();
        let other_for_keys = other.clone();
        let our_keys = self.keys.clone();
        let result = Arguments::new(
            Arc::new(move |key| us.get(key).or_else(|| other.get(key))),
            Arc::new(move || {
                let mut seen = HashSet::new();
                (other_for_keys.keys)().into_iter().chain(our_keys()).filter(|k| seen.insert(k.clone())).collect()
            }),
        );
        if self.silenced() {
            result.silence();
        }
        result
    }

    /// Returns the trimmed value of an argument without logging it.
    pub fn get_arg(&self, key: &str) -> Option<String> {
        self.get(key).map(|v| v.trim().to_string())
    }

    /// Returns the trimmed value of an argument, or `default_value` if missing, without logging it.
    pub fn get_arg_or(&self, key: &str, default_value: &str) -> String {
        self.get_arg(key).unwrap_or_else(|| default_value.to_string())
    }

    /// Returns an envelope parsed from the `key` argument (format: `westLng,southLat,eastLng,northLat`, or `world` /
    /// `planet`), or `None` if missing.
    pub fn bounds(&self, key: &str, description: &str) -> anyhow::Result<Option<Envelope>> {
        let input = self.get_arg(key);
        let result = match input.as_deref() {
            Some(s) if s.eq_ignore_ascii_case("world") || s.eq_ignore_ascii_case("planet") => {
                Some(*geo_utils::WORLD_LAT_LON_BOUNDS)
            }
            Some(s) => {
                let bounds = s
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .filter(|p| !p.is_empty())
                    .map(|p| p.parse::<f64>().with_context(|| format!("invalid bounds coordinate: {p}")))
                    .collect::<anyhow::Result<Vec<f64>>>()?;
                if bounds.len() != 4 {
                    bail!("bounds must have 4 coordinates, got: {s}");
                }
                Some(Envelope::new(bounds[0], bounds[2], bounds[1], bounds[3]))
            }
            None => None,
        };
        self.log_arg_value(
            key,
            description,
            &result.as_ref().map_or("null".to_string(), geo_utils::envelope_to_string),
        );
        Ok(result)
    }

    fn log_arg_value(&self, key: &str, description: &str, result: &dyn fmt::Display) {
        if let Some(logged) = &self.log_once {
            let mut logged = logged.lock();
            let count = logged.entry(key.to_string()).or_insert(0);
            *count += 1;
            if *count == 3000 {
                tracing::warn!("Too many requests for argument '{key}', result should be cached");
            }
            if *count != 1 {
                return;
            }
        }
        if !self.silenced() && tracing::enabled!(tracing::Level::DEBUG) {
            let key = key.split('|').next().unwrap_or(key);
            tracing::debug!("argument: {key}={result} ({description})");
        }
    }

    /// Stop logging argument values when they are read, and return this instance.
    pub fn silence(&self) -> Arguments {
        self.silent.store(true, Ordering::Relaxed);
        self.clone()
    }

    /// Returns true if [`Arguments::silence`] was called.
    pub fn silenced(&self) -> bool {
        self.silent.load(Ordering::Relaxed)
    }

    /// Returns the value of `key`, or `default_value` if missing.
    pub fn get_string(&self, key: &str, description: &str, default_value: &str) -> String {
        let value = self.get_arg_or(key, default_value);
        self.log_arg_value(key, description, &value);
        value
    }

    /// Returns the value of `key`, or `None` if missing (Java's `getString(key, description, null)`).
    pub fn get_string_opt(&self, key: &str, description: &str) -> Option<String> {
        let value = self.get_arg(key);
        self.log_arg_value(key, description, &value.as_deref().unwrap_or("null"));
        value
    }

    /// Returns the value of a required argument `key`.
    pub fn get_required_string(&self, key: &str, description: &str) -> anyhow::Result<String> {
        let value = self.get_required_arg(key, description)?;
        self.log_arg_value(key, description, &value);
        Ok(value)
    }

    fn get_required_arg(&self, key: &str, description: &str) -> anyhow::Result<String> {
        self.get_arg(key).ok_or_else(|| anyhow::anyhow!("Missing required parameter: {key} ({description})"))
    }

    /// Returns a path parsed from `key`, or `default_value` if the argument is not set.
    pub fn file(&self, key: &str, description: &str, default_value: impl Into<PathBuf>) -> PathBuf {
        let file = self.get_arg(key).map_or_else(|| default_value.into(), PathBuf::from);
        self.log_arg_value(key, description, &file.display());
        file
    }

    /// Returns a path parsed from `key`, or `None` if the argument is not set.
    pub fn file_opt(&self, key: &str, description: &str) -> Option<PathBuf> {
        let file = self.get_arg(key).map(PathBuf::from);
        self.log_arg_value(key, description, &file.as_ref().map_or("null".to_string(), |f| f.display().to_string()));
        file
    }

    /// Returns a path parsed from a required `key` argument which may or may not exist.
    pub fn required_file(&self, key: &str, description: &str) -> anyhow::Result<PathBuf> {
        let file = PathBuf::from(self.get_required_arg(key, description)?);
        self.log_arg_value(key, description, &file.display());
        Ok(file)
    }

    /// Returns a path parsed from `key` (or `default_value`) which must exist.
    pub fn input_file(
        &self,
        key: &str,
        description: &str,
        default_value: impl Into<PathBuf>,
    ) -> anyhow::Result<PathBuf> {
        let path = self.file(key, description, default_value);
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        Ok(path)
    }

    /// Returns a path parsed from a required `key` argument which must exist.
    pub fn required_input_file(&self, key: &str, description: &str) -> anyhow::Result<PathBuf> {
        let path = self.required_file(key, description)?;
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        Ok(path)
    }

    /// Returns a boolean parsed from `key` where `"true"` (case-insensitive) is true and anything else is false.
    pub fn get_boolean(&self, key: &str, description: &str, default_value: bool) -> bool {
        let value = self.get_arg(key).map_or(default_value, |v| v.eq_ignore_ascii_case("true"));
        self.log_arg_value(key, description, &value);
        value
    }

    /// Returns a boolean parsed from `key` or `None` if not specified.
    pub fn get_boolean_object(&self, key: &str, description: &str) -> Option<bool> {
        let value = self.get_arg(key).map(|v| v.eq_ignore_ascii_case("true"));
        self.log_arg_value(key, description, &value.map_or("null".to_string(), |v| v.to_string()));
        value
    }

    /// Returns a list parsed from `key` where values are separated by commas.
    pub fn get_list<S: AsRef<str>>(&self, key: &str, description: &str, default_value: &[S]) -> Vec<String> {
        let default = default_value.iter().map(|s| s.as_ref()).collect::<Vec<_>>().join(",");
        let value = self.get_arg_or(key, &default);
        let results = value.split(',').map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect();
        self.log_arg_value(key, description, &value);
        results
    }

    /// Returns the number of threads from the number of available processors, but allows the user to override it by
    /// setting the `threads` argument (minimum 2).
    pub fn threads(&self) -> anyhow::Result<usize> {
        let value = self.get_arg_or("threads", &num_cpus::get().to_string());
        let threads: i64 = value.parse().with_context(|| format!("invalid threads: {value}"))?;
        let threads = threads.max(2) as usize;
        self.log_arg_value("threads", "num threads", &threads);
        Ok(threads)
    }

    /// Returns a stats implementation based on the arguments provided.
    ///
    /// If `pushgateway` is set then it uses a stats implementation that pushes to prometheus through a push gateway
    /// every `pushgateway.interval`. Otherwise, uses an in-memory stats implementation.
    pub fn get_stats(&self) -> anyhow::Result<StatsRef> {
        match self.get_arg("pushgateway").filter(|p| !p.trim().is_empty()) {
            Some(prometheus) => {
                tracing::info!("argument: stats=use prometheus push gateway stats");
                let job = self.get_string("pushgateway.job", "prometheus pushgateway job ID", "planetiler");
                let interval = self.get_duration(
                    "pushgateway.interval",
                    "how often to send stats to prometheus push gateway",
                    "15s",
                )?;
                crate::stats::stats::prometheus_push_gateway(&prometheus, &job, interval)
            }
            None => {
                tracing::info!("argument: stats=use in-memory stats");
                Ok(crate::stats::stats::in_memory())
            }
        }
    }

    /// Returns an argument as an integer, where `"false"` means 0 and `"true"` means `default_value`.
    pub fn get_integer(&self, key: &str, description: &str, default_value: i32) -> anyhow::Result<i32> {
        let value = self.get_arg_or(key, &default_value.to_string());
        let parsed = match value.to_lowercase().as_str() {
            "false" => 0,
            "true" => default_value,
            _ => value.parse().with_context(|| format!("invalid integer for {key}: {value}"))?,
        };
        self.log_arg_value(key, description, &parsed);
        Ok(parsed)
    }

    /// Returns an argument as a double.
    pub fn get_double(&self, key: &str, description: &str, default_value: f64) -> anyhow::Result<f64> {
        let parsed = match self.get_arg(key) {
            None => default_value,
            Some(value) => parse_java_double(&value).with_context(|| format!("invalid number for {key}: {value}"))?,
        };
        self.log_arg_value(key, description, &parsed);
        Ok(parsed)
    }

    /// Returns an argument as a duration (i.e. "10s", "90m", "1h30m", or ISO-8601 like "PT1H").
    pub fn get_duration(&self, key: &str, description: &str, default_value: &str) -> anyhow::Result<Duration> {
        let value = self.get_arg_or(key, default_value);
        let parsed = parse_duration(&value).with_context(|| format!("invalid duration for {key}: {value}"))?;
        self.log_arg_value(key, description, &format!("{} seconds", parsed.as_secs()));
        Ok(parsed)
    }

    /// Returns an argument as a long.
    pub fn get_long(&self, key: &str, description: &str, default_value: i64) -> anyhow::Result<i64> {
        let value = self.get_arg_or(key, &default_value.to_string());
        let parsed = value.parse().with_context(|| format!("invalid long for {key}: {value}"))?;
        self.log_arg_value(key, description, &parsed);
        Ok(parsed)
    }

    /// Returns an argument converted with `converter`, or `default_value` if missing.
    pub fn get_object<T: fmt::Debug>(
        &self,
        key: &str,
        description: &str,
        default_value: T,
        converter: impl FnOnce(&str) -> T,
    ) -> T {
        let value = self.get_arg(key).map_or(default_value, |s| converter(&s));
        self.log_arg_value(key, description, &format!("{value:?}"));
        value
    }

    /// Returns a map from all the arguments provided to their values.
    pub fn to_map(&self) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        for key in (self.keys)() {
            if let Some(value) = self.get(&key) {
                result.insert(normalize(&key), value);
            }
        }
        result
    }

    /// Returns a copy of this instance that logs each extracted argument value exactly once.
    pub fn with_exactly_once_logging(&self) -> Arguments {
        Arguments {
            provider: self.provider.clone(),
            keys: self.keys.clone(),
            silent: Arc::new(AtomicBool::new(self.silenced())),
            log_once: Some(Arc::new(Mutex::new(HashMap::new()))),
        }
    }

    /// Returns a copy of this instance that is not silenced and logs every value.
    pub fn copy(&self) -> Arguments {
        Arguments::new(self.provider.clone(), self.keys.clone())
    }

    /// Returns a new arguments instance that translates requests for `"key"` to `"prefix_key"`.
    pub fn with_prefix(&self, prefix: &str) -> Arguments {
        Self::from_prefixed(self.provider.clone(), self.keys.clone(), prefix, "_", false)
    }

    /// Returns a view of this instance that only supports requests for `allowed_keys`.
    pub fn subset(&self, allowed_keys: &[&str]) -> Arguments {
        let allowed: Arc<HashSet<String>> = Arc::new(allowed_keys.iter().map(|k| normalize(k)).collect());
        let (allowed2, provider, keys) = (allowed.clone(), self.provider.clone(), self.keys.clone());
        Arguments::new(
            Arc::new(move |key| if allowed.contains(&normalize(key)) { provider(key) } else { None }),
            Arc::new(move || keys().into_iter().filter(|k| allowed2.contains(&normalize(k))).collect()),
        )
    }

    /// Returns a new arguments instance where the value for `key` defaults to `value`.
    pub fn with_default(&self, key: impl ToString, value: impl ToString) -> Arguments {
        let key = key.to_string();
        self.or_else(Arguments::of_pairs([(key.trim_start_matches('-'), value.to_string())]))
    }
}

/// Parses a number like Java's `Double.parseDouble` (also accepting `Infinity`/`NaN` and a trailing `d`/`f`).
fn parse_java_double(s: &str) -> anyhow::Result<f64> {
    let s = s.trim();
    let unsigned = s.trim_start_matches(['+', '-']);
    let neg = s.starts_with('-');
    match unsigned {
        "Infinity" => return Ok(if neg { f64::NEG_INFINITY } else { f64::INFINITY }),
        "NaN" => return Ok(f64::NAN),
        _ => {}
    }
    let trimmed = s.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(s);
    Ok(trimmed.parse()?)
}

/// Parses a duration like `10s`, `1h30m`, `500ms`-free ISO-8601 (`PT1H30M`, `P1DT2H`), like Java's
/// `Duration.parse` with an implicit `PT` prefix.
pub fn parse_duration(value: &str) -> anyhow::Result<Duration> {
    let mut value = value.trim().to_string();
    if !value.starts_with(['P', 'p', 'T', 't', '-']) {
        value = format!("PT{value}");
    }
    let upper = value.to_uppercase();
    let mut s = upper.as_str();
    if s.starts_with('-') {
        bail!("negative durations are not supported: {value}");
    }
    s = s.strip_prefix('+').unwrap_or(s);
    s = s.strip_prefix('P').ok_or_else(|| anyhow::anyhow!("invalid duration: {value}"))?;
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut total = 0f64;
    let mut parsed_any = false;
    let mut parse_part = |part: &str, units: &[(char, f64)]| -> anyhow::Result<()> {
        let mut num = String::new();
        let mut unit_idx = 0;
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' || c == ',' || c == '-' || c == '+' {
                num.push(if c == ',' { '.' } else { c });
            } else {
                let pos = units[unit_idx..]
                    .iter()
                    .position(|(u, _)| *u == c)
                    .ok_or_else(|| anyhow::anyhow!("invalid duration unit '{c}' in {value}"))?;
                unit_idx += pos;
                let n: f64 = num.parse().with_context(|| format!("invalid duration: {value}"))?;
                if n.fract() != 0.0 && units[unit_idx].0 != 'S' {
                    bail!("only seconds can be fractional in duration: {value}");
                }
                total += n * units[unit_idx].1;
                unit_idx += 1;
                parsed_any = true;
                num.clear();
            }
        }
        if !num.is_empty() {
            bail!("invalid duration, missing unit: {value}");
        }
        Ok(())
    };
    parse_part(date_part, &[('D', 86400.0)])?;
    if let Some(time_part) = time_part {
        if time_part.is_empty() {
            bail!("invalid duration: {value}");
        }
        parse_part(time_part, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)])?;
    }
    if !parsed_any {
        bail!("invalid duration: {value}");
    }
    if total < 0.0 {
        bail!("negative durations are not supported: {value}");
    }
    Ok(Duration::from_secs_f64(total))
}

/// Parses the contents of a Java `.properties` file.
///
/// Supports `#`/`!` comments, `key=value`, `key:value` and `key value` separators, line continuations ending with
/// `\`, and backslash escapes including `\uXXXX`.
pub fn parse_properties(contents: &str) -> IndexMap<String, String> {
    let mut result = IndexMap::new();
    let mut lines = contents.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }
        let mut logical = trimmed.to_string();
        while ends_with_continuation(&logical) {
            logical.pop();
            match lines.next() {
                Some(next) => logical.push_str(next.trim_start()),
                None => break,
            }
        }
        let chars: Vec<char> = logical.chars().collect();
        let mut i = 0;
        let mut key_raw = String::new();
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() {
                key_raw.push(c);
                key_raw.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '=' || c == ':' || c.is_whitespace() {
                break;
            }
            key_raw.push(c);
            i += 1;
        }
        // skip whitespace, then at most one separator, then whitespace
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && (chars[i] == '=' || chars[i] == ':') {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let value_raw: String = chars[i..].iter().collect();
        result.insert(unescape_properties(&key_raw), unescape_properties(&value_raw));
    }
    result
}

fn ends_with_continuation(s: &str) -> bool {
    s.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1
}

fn unescape_properties(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn test_properties() -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "# used for ArgumentsTest\nkey1=value1fromfile\nkey2=value2fromfile").unwrap();
        file
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn empty() {
        assert_eq!("fallback", Arguments::of().get_string("key", "key", "fallback"));
    }

    #[test]
    fn map_based() {
        assert_eq!("value", Arguments::of_pairs([("key", "value")]).get_string("key", "key", "fallback"));
    }

    #[test]
    fn or_else() {
        let args = Arguments::of_pairs([("key1", "value1a"), ("key2", "value2a")])
            .or_else(Arguments::of_pairs([("key2", "value2b"), ("key3", "value3b")]));
        assert_eq!("value1a", args.get_string("key1", "key", "fallback"));
        assert_eq!("value2a", args.get_string("key2", "key", "fallback"));
        assert_eq!("value3b", args.get_string("key3", "key", "fallback"));
        assert_eq!("fallback", args.get_string("key4", "key", "fallback"));
    }

    #[test]
    fn with_fallback() {
        let args = Arguments::of_pairs([("key1", "value1a"), ("key2", "value2a")])
            .with_default("key2", "value2b")
            .with_default("--key3", "value3b")
            .with_default("key_5", true);
        assert_eq!("value1a", args.get_string("key1", "key", "fallback"));
        assert_eq!("value2a", args.get_string("key2", "key", "fallback"));
        assert_eq!("value3b", args.get_string("key3", "key", "fallback"));
        assert_eq!("fallback", args.get_string("key4", "key", "fallback"));
        assert!(args.get_boolean("key-5", "key", false));
    }

    #[test]
    fn config_file_parsing() {
        let file = test_properties();
        let args = Arguments::from_config_file(file.path()).unwrap();
        assert_eq!("value1fromfile", args.get_string("key1", "key", "fallback"));
        assert_eq!("fallback", args.get_string("key3", "key", "fallback"));
    }

    #[test]
    fn get_config_file_from_args() {
        let file = test_properties();
        let args = Arguments::from_args_or_config_file(&[
            format!("config={}", file.path().display()),
            "key2=value2fromargs".to_string(),
        ])
        .unwrap();
        assert_eq!("value1fromfile", args.get_string("key1", "key", "fallback"));
        assert_eq!("value2fromargs", args.get_string("key2", "key", "fallback"));
        assert_eq!("fallback", args.get_string("key3", "key", "fallback"));
    }

    #[test]
    fn defaults_missing_config_file() {
        let args = Arguments::from_args_or_config_file(&["key=value"]).unwrap();
        assert_eq!("value", args.get_string("key", "key", "fallback"));
        assert_eq!("fallback", args.get_string("key2", "key", "fallback"));
    }

    #[test]
    fn duration() {
        let args = Arguments::of_pairs([("duration", "1h30m")]);
        assert_eq!(Duration::from_secs(90 * 60), args.get_duration("duration", "key", "10m").unwrap());
        assert_eq!(Duration::from_secs(10), args.get_duration("duration2", "key", "10s").unwrap());
        assert_eq!(Duration::from_secs(3600), parse_duration("PT1H").unwrap());
        assert_eq!(Duration::from_secs(86400 + 7200), parse_duration("P1DT2H").unwrap());
        assert_eq!(Duration::from_millis(1500), parse_duration("1.5s").unwrap());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("10m1h").is_err());
    }

    #[test]
    fn integer() {
        let args = Arguments::of_pairs([("integer", "30")]);
        assert_eq!(30, args.get_integer("integer", "key", 10).unwrap());
        assert_eq!(10, args.get_integer("integer2", "key", 10).unwrap());
        assert!(Arguments::of_pairs([("integer", "x")]).get_integer("integer", "key", 10).is_err());
    }

    #[test]
    fn long() {
        let max_int = i32::MAX as i64;
        let args = Arguments::of_pairs([("long", (max_int * 2).to_string())]);
        assert_eq!(max_int * 2, args.get_long("long", "key", max_int + 1).unwrap());
        assert_eq!(max_int + 1, args.get_long("long2", "key", max_int + 1).unwrap());
    }

    #[test]
    fn double() {
        let args = Arguments::of_pairs([("d", "1.5"), ("inf", "Infinity")]);
        assert_eq!(1.5, args.get_double("d", "key", 1.0).unwrap());
        assert_eq!(f64::INFINITY, args.get_double("inf", "key", 1.0).unwrap());
        assert_eq!(2.0, args.get_double("missing", "key", 2.0).unwrap());
    }

    #[test]
    fn threads() {
        assert_eq!(2, Arguments::of_pairs([("threads", "2")]).threads().unwrap());
        assert_eq!(2, Arguments::of_pairs([("threads", "1")]).threads().unwrap());
        assert!(Arguments::of().threads().unwrap() > 0);
    }

    #[test]
    fn list() {
        assert_eq!(vec!["1", "2", "3"], Arguments::of_pairs([("list", "1,2,3")]).get_list("list", "list", &["1"]));
        assert_eq!(vec!["1"], Arguments::of().get_list("list", "list", &["1"]));
        assert!(Arguments::of().get_list::<&str>("list", "list", &[]).is_empty());
    }

    #[test]
    fn boolean() {
        assert!(Arguments::of_pairs([("boolean", "true")]).get_boolean("boolean", "list", false));
        assert!(!Arguments::of_pairs([("boolean", "false")]).get_boolean("boolean", "list", true));
        assert!(!Arguments::of_pairs([("boolean", "true1")]).get_boolean("boolean", "list", true));
        assert!(!Arguments::of().get_boolean("boolean", "list", false));
    }

    #[test]
    fn file() {
        let props = test_properties();
        let path = props.path().display().to_string();
        assert!(Arguments::of_pairs([("file", path.as_str())]).input_file("file", "file", "").is_ok());
        let missing = format!("{path}.X");
        assert!(Arguments::of_pairs([("file", missing.as_str())]).input_file("file", "file", "").is_err());
        assert_eq!(PathBuf::from(&missing), Arguments::of_pairs([("file", missing.as_str())]).file("file", "file", ""));
        assert!(Arguments::of().required_file("file", "file").is_err());
        assert_eq!(None, Arguments::of().file_opt("file", "file"));
    }

    #[test]
    fn bounds() {
        use crate::config::bounds::Bounds;
        assert_eq!(
            Envelope::new(1.0, 3.0, 2.0, 4.0),
            Bounds::new(Arguments::of_pairs([("bounds", "1,2,3,4")]).bounds("bounds", "bounds").unwrap()).lat_lon()
        );
        let world =
            Bounds::new(Arguments::of_pairs([("bounds", "world")]).bounds("bounds", "bounds").unwrap()).lat_lon();
        approx::assert_relative_eq!(-180.0, world.min_x);
        approx::assert_relative_eq!(180.0, world.max_x);
        approx::assert_relative_eq!(-85.0511287798066, world.min_y, epsilon = 1e-9);
        approx::assert_relative_eq!(85.0511287798066, world.max_y, epsilon = 1e-9);
        assert_eq!(None, Arguments::of().bounds("bounds", "bounds").unwrap());
        assert!(Arguments::of_pairs([("bounds", "1,2,3")]).bounds("bounds", "bounds").is_err());
        // TODO(port): Java also tests falling back to the bounds of monaco-latest.osm.pbf via OsmInputFile
    }

    #[test]
    fn stats() {
        let _lock = crate::stats::default_stats::TEST_LOCK.lock();
        assert!(Arguments::of().get_stats().is_ok());
    }

    #[test]
    fn args_key_present_implies() {
        assert!(Arguments::from_args(&["--force"]).get_boolean("force", "force", false));
    }

    #[test]
    fn underscore_dash_same() {
        assert!(Arguments::from_args(&["--force-down-load=true"]).get_boolean("force_down_load", "force", false));
        assert!(Arguments::from_args(&["--force-download=true"]).get_boolean("force_download", "force", false));
        assert!(Arguments::from_args(&["--force_download=true"]).get_boolean("force-download", "force", false));
        assert!(Arguments::from_args(&["--force_download=true"]).get_boolean("force_download", "force", false));
    }

    #[test]
    fn space_between_args() {
        let args: Vec<&str> = "--key value --key2 value2 --force1 --force2".split_whitespace().collect();
        let args = Arguments::from_args(&args);
        assert_eq!(Some("value".to_string()), args.get_string_opt("key", "key"));
        assert_eq!(Some("value2".to_string()), args.get_string_opt("key2", "key2"));
        assert!(args.get_boolean("force1", "force1", false));
        assert!(args.get_boolean("force2", "force2", false));
    }

    #[test]
    fn list_argument_values_from_command_line() {
        assert!(Arguments::from_args::<&str>(&[]).to_map().is_empty());
        let args: Vec<&str> = "--key value --key2 value2 --force1 --force2".split_whitespace().collect();
        assert_eq!(
            map(&[("key", "value"), ("key2", "value2"), ("force1", "true"), ("force2", "true")]),
            Arguments::from_args(&args).to_map()
        );
    }

    #[test]
    fn list_argument_values_from_map() {
        assert!(Arguments::of_map(Vec::<(String, String)>::new()).to_map().is_empty());
        assert_eq!(map(&[("a", "1"), ("b", "2")]), Arguments::of_pairs([("a", "1"), ("b", "2")]).to_map());
    }

    #[test]
    fn list_argument_values_from_config_file() {
        let file = test_properties();
        let args = Arguments::from_config_file(file.path()).unwrap();
        assert_eq!(map(&[("key1", "value1fromfile"), ("key2", "value2fromfile")]), args.to_map());
    }

    fn env_args(pairs: &[(&str, &str)], jvm: bool) -> Arguments {
        let env: Arc<HashMap<String, String>> =
            Arc::new(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        let env2 = env.clone();
        let getter = move |k: &str| env.get(k).cloned();
        let keys = move || env2.keys().cloned().collect();
        if jvm {
            Arguments::from_jvm_properties_with(getter, keys)
        } else {
            Arguments::from_environment_with(getter, keys)
        }
    }

    #[test]
    fn list_arguments_from_environment() {
        let args = env_args(
            &[
                ("OTHER", "value"),
                ("PLANETILEROTHER", "VALUE"),
                ("PLANETILER_KEY1", "value1"),
                ("PLANETILER_KEY2", "value2"),
            ],
            false,
        );
        assert_eq!(map(&[("key1", "value1"), ("key2", "value2")]), args.to_map());
    }

    #[test]
    fn list_arguments_from_jvm_properties() {
        let args = env_args(
            &[
                ("OTHER", "value"),
                ("PLANETILEROTHER", "VALUE"),
                ("PLANETILER_KEY1", "value1"),
                ("PLANETILER_KEY2", "value2"),
                ("planetiler.key3", "value4"),
            ],
            true,
        );
        assert_eq!(map(&[("key3", "value4")]), args.to_map());
    }

    #[test]
    fn list_arguments_from_merged() {
        let env = env_args(
            &[
                ("OTHER", "value"),
                ("PLANETILEROTHER", "VALUE"),
                ("PLANETILER_KEY1", "value1"),
                ("PLANETILER_KEY2", "value2"),
                ("planetiler.key3", "value3"),
            ],
            false,
        );
        let jvm = env_args(
            &[
                ("other", "value"),
                ("PLANETILEROTHER", "VALUE"),
                ("PLANETILER_KEY1", "value1"),
                ("PLANETILER_KEY2", "value2"),
                ("planetiler.key3", "value4"),
            ],
            true,
        );
        let args = jvm.or_else(env);
        assert_eq!(map(&[("key3", "value4"), ("key1", "value1"), ("key2", "value2")]), args.to_map());
    }

    #[test]
    #[should_panic(expected = "expected")]
    fn dont_access_arg_list_until_used() {
        let env: HashMap<String, String> =
            [("PLANETILER_KEY1", "value1")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let args = Arguments::from_environment_with(move |k| env.get(k).cloned(), || panic!("expected"));
        assert_eq!("value1", args.get_required_string("key1", "").unwrap());
        args.to_map();
    }

    #[test]
    fn boolean_object() {
        let args = Arguments::of_pairs([("BOOL_TRUE", "true"), ("BOOL_FALSE", "false"), ("BOOL_NO", "no")]);
        assert_eq!(None, args.get_boolean_object("BOOL_NULL", "test"));
        assert_eq!(Some(true), args.get_boolean_object("BOOL_TRUE", "test"));
        assert_eq!(Some(false), args.get_boolean_object("BOOL_FALSE", "test"));
        assert_eq!(Some(false), args.get_boolean_object("BOOL_NO", "test"));
    }

    #[test]
    fn deprecated_args() {
        assert_eq!(
            "newvalue",
            Arguments::of_pairs([("oldkey", "oldvalue"), ("newkey", "newvalue")]).get_string(
                "newkey|oldkey",
                "key",
                "fallback"
            )
        );
        assert_eq!(
            "oldvalue",
            Arguments::of_pairs([("oldkey", "oldvalue")]).get_string("newkey|oldkey", "key", "fallback")
        );
        assert_eq!("fallback", Arguments::of().get_string("newkey|oldkey", "key", "fallback"));
    }

    #[test]
    fn with_prefix() {
        let args = Arguments::of_pairs([("prefix_a", "a_val"), ("prefix-b", "b_val"), ("other", "other_val")])
            .with_prefix("prefix");
        assert_eq!(Some("a_val".to_string()), args.get_arg("a"));
        assert_eq!(Some("b_val".to_string()), args.get_arg("b"));
        assert_eq!(None, args.get_arg("other"));
        assert_eq!(None, args.get_arg("prefix_a"));
        assert_eq!(None, args.get_arg("prefix_b"));
        assert_eq!(None, args.get_arg("prefix_other"));
        assert_eq!(vec!["a", "b"], args.to_map().into_keys().collect::<Vec<_>>());
    }

    #[test]
    fn prefix_from_environment() {
        let args = env_args(
            &[
                ("OTHER", "value"),
                ("PLANETILEROTHER", "VALUE"),
                ("PLANETILER_MBTILES_KEY1", "value1"),
                ("PLANETILER_PMTILES_KEY2", "value2"),
            ],
            false,
        )
        .with_prefix("mbtiles");
        assert_eq!(map(&[("key1", "value1")]), args.to_map());
        assert_eq!(Some("value1".to_string()), args.get_arg("key1"));
    }

    #[test]
    fn subset() {
        let args = Arguments::of_pairs([("key_1", "val_1"), ("key-2", "val_2"), ("key-3", "val_3")])
            .subset(&["key-1", "key-2"]);
        assert_eq!(map(&[("key_1", "val_1"), ("key_2", "val_2")]), args.to_map());
        assert_eq!(Some("val_1".to_string()), args.get_arg("key-1"));
        assert_eq!(None, args.get_arg("key-3"));
    }

    #[test]
    fn false_for_int() {
        let args = Arguments::of_pairs([("true", "true"), ("false", "false"), ("3", "3")]);
        assert_eq!(1, args.get_integer("true", "", 1).unwrap());
        assert_eq!(0, args.get_integer("false", "", 1).unwrap());
        assert_eq!(3, args.get_integer("3", "", 1).unwrap());
    }

    #[test]
    fn exactly_once_logging_and_silence() {
        let args = Arguments::of_pairs([("a", "1")]).with_exactly_once_logging();
        for _ in 0..5 {
            assert_eq!("1", args.get_string("a", "a", "0"));
        }
        let silent = Arguments::of().silence();
        assert!(silent.silenced());
        assert!(silent.or_else(Arguments::of()).silenced());
        assert!(!silent.copy().silenced());
    }

    #[test]
    fn properties_format() {
        let props = parse_properties("# comment\n! comment\na = 1\nb:2\nc 3\nd=multi\\\n  line\ne=\\u0041\\tb\n  f=  ");
        assert_eq!("1", props["a"]);
        assert_eq!("2", props["b"]);
        assert_eq!("3", props["c"]);
        assert_eq!("multiline", props["d"]);
        assert_eq!("A\tb", props["e"]);
        assert_eq!("", props["f"]);
    }
}
