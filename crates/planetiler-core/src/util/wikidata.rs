//! Port of `util/Wikidata.java`: a utility to download name translations from wikidata for all OSM elements with a
//! [wikidata tag](https://wiki.openstreetmap.org/wiki/Key:wikidata).
//!
//! Java takes a `Profile` and calls `profile.caresAboutWikidataTranslation(elem)`; here the core functions take a
//! filter closure and [`profile_filter`] / [`fetch_for_profile`] adapt a [`Profile`](crate::profile::Profile).

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value as Json;

use crate::config::planetiler_config::PlanetilerConfig;
use crate::reader::osm::osm_block_source::OsmBlockSource;
use crate::reader::osm::osm_element::OsmElement;
use crate::reader::osm::osm_input_file::OsmInputFile;
use crate::reader::with_tags::WithTags;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::util::format;
use crate::util::translations::TranslationProvider;
use crate::value::{Tags, Value};
use crate::worker::worker_pipeline::WorkerPipeline;

static WIKIDATA_IRI_MATCHER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^http://www\.wikidata\.org/entity/Q(\d+)$").unwrap());
static QID_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Q(\d+)$").unwrap());
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Wikidata's SPARQL endpoint.
pub const SPARQL_URL: &str = "https://query.wikidata.org/bigdata/namespace/wdq/sparql";

/// Map from language to name for each wikidata QID.
pub type TranslationMap = HashMap<i64, BTreeMap<String, String>>;

/// An HTTP request to the wikidata SPARQL endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikidataRequest {
    pub url: String,
    pub method: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl WikidataRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Abstraction over HTTP client so tests can easily mock-out the client's response (Java's `Wikidata.Client`).
pub trait WikidataClient: Send {
    /// Sends `request` and returns the (decompressed) response body, or an error for failures and HTTP status >= 400.
    fn send(&self, request: &WikidataRequest) -> anyhow::Result<Vec<u8>>;
}

impl<F: Fn(&WikidataRequest) -> anyhow::Result<Vec<u8>> + Send> WikidataClient for F {
    fn send(&self, request: &WikidataRequest) -> anyhow::Result<Vec<u8>> {
        self(request)
    }
}

/// [`WikidataClient`] backed by `ureq` (handles gzip `Content-Encoding` transparently).
pub struct UreqWikidataClient {
    agent: ureq::Agent,
}

impl UreqWikidataClient {
    pub fn new(config: &PlanetilerConfig) -> UreqWikidataClient {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(config.http_timeout))
            .timeout_global(Some(config.http_timeout))
            .http_status_as_error(false)
            .build()
            .into();
        UreqWikidataClient { agent }
    }
}

impl WikidataClient for UreqWikidataClient {
    fn send(&self, request: &WikidataRequest) -> anyhow::Result<Vec<u8>> {
        let mut builder = self.agent.post(&request.url);
        for (k, v) in &request.headers {
            builder = builder.header(k, v);
        }
        let response = builder.send(request.body.as_bytes())?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        let read = response.into_body().into_reader().read_to_end(&mut body);
        if status >= 400 {
            let body = match read {
                Ok(_) => String::from_utf8_lossy(&body).to_string(),
                Err(e) => format!("Error reading body: {e}"),
            };
            bail!(std::io::Error::other(format!("Error reading {status}: {body}")));
        }
        read?;
        Ok(body)
    }
}

/// Returns a numeric ID from a wikidata IRI like `http://www.wikidata.org/entity/Q9141`.
fn extract_id_from_wikidata_iri(iri: &str) -> anyhow::Result<i64> {
    match WIKIDATA_IRI_MATCHER.captures(iri) {
        Some(c) => Ok(c[1].parse()?),
        None => bail!("Unexpected response IRI: {iri}"),
    }
}

/// Returns a numeric ID from a wikidata ID starting with a "Q", or 0 if it is not a QID.
pub fn parse_qid(qid: &str) -> i64 {
    QID_PATTERN.captures(qid).and_then(|c| c[1].parse().ok()).unwrap_or(0)
}

/// Parses SPARQL results JSON and returns a map from QID to language to name.
fn parse_results(results: &[u8]) -> anyhow::Result<TranslationMap> {
    let node: Json = serde_json::from_slice(results)?;
    let bindings =
        node.get("results").and_then(|r| r.get("bindings")).and_then(Json::as_array).context("missing bindings")?;
    let mut result = TranslationMap::new();
    let text = |v: Option<&Json>| match v {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    for row in bindings {
        let id = extract_id_from_wikidata_iri(&text(row.get("id").and_then(|id| id.get("value"))))?;
        let label = row.get("label");
        result
            .entry(id)
            .or_default()
            .insert(text(label.and_then(|l| l.get("xml:lang"))), text(label.and_then(|l| l.get("value"))));
    }
    Ok(result)
}

fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Counters shared with progress loggers.
#[derive(Default)]
struct Counters {
    blocks: AtomicI64,
    nodes: AtomicI64,
    ways: AtomicI64,
    rels: AtomicI64,
    wikidatas: AtomicI64,
    batches: AtomicI64,
}

/// Downloads wikidata translations in batches and writes them to `writer`.
pub struct Wikidata<W: Write> {
    counters: Arc<Counters>,
    visited: HashSet<i64>,
    qids_to_fetch: Vec<i64>,
    writer: W,
    client: Box<dyn WikidataClient>,
    batch_size: usize,
    http_user_agent: String,
    http_retries: u32,
    http_retry_wait: Duration,
    sleep: fn(Duration),
}

impl<W: Write> Wikidata<W> {
    pub fn new(writer: W, client: Box<dyn WikidataClient>, batch_size: usize, config: &PlanetilerConfig) -> Self {
        Wikidata {
            counters: Arc::default(),
            visited: HashSet::new(),
            qids_to_fetch: Vec::with_capacity(batch_size),
            writer,
            client,
            batch_size,
            http_user_agent: config.http_user_agent.clone(),
            http_retries: config.http_retries,
            http_retry_wait: config.http_retry_wait,
            sleep: std::thread::sleep,
        }
    }

    /// Fetches translations for the pending batch and writes them to the output.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        let start = Instant::now();
        let results = self.query_wikidata()?;
        let batches = self.counters.batches.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            "Fetched batch {batches} ({} qids) {}",
            self.qids_to_fetch.len(),
            format::duration(start.elapsed())
        );
        self.write_translations(&results, &HashMap::new())?;
        self.counters.wikidatas.fetch_add(self.qids_to_fetch.len() as i64, Ordering::Relaxed);
        self.qids_to_fetch.clear();
        Ok(())
    }

    /// Queues `id` to be fetched (unless it was already fetched), flushing when the batch is full.
    pub fn fetch(&mut self, id: i64) -> anyhow::Result<()> {
        if self.visited.insert(id) {
            self.qids_to_fetch.push(id);
        }
        if self.qids_to_fetch.len() >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Make an HTTP request to wikidata's
    /// [sparql endpoint](https://www.wikidata.org/wiki/Wikidata:SPARQL_query_service) to fetch name translations for
    /// the pending set of QIDs.
    fn query_wikidata(&self) -> anyhow::Result<TranslationMap> {
        if self.qids_to_fetch.is_empty() {
            return Ok(TranslationMap::new());
        }
        let qid_list = self.qids_to_fetch.iter().map(|id| format!("wd:Q{id}")).collect::<Vec<_>>().join(" ");
        let query = format!(
            "SELECT ?id ?label where {{\n  VALUES ?id {{ {qid_list} }} ?id (owl:sameAs* / rdfs:label) ?label\n}}\n"
        );
        let query = WHITESPACE.replace_all(&query, " ").trim().to_string();
        let request = WikidataRequest {
            url: SPARQL_URL.to_string(),
            method: "POST",
            headers: vec![
                ("User-Agent".to_string(), self.http_user_agent.clone()),
                ("Accept".to_string(), "application/sparql-results+json".to_string()),
                ("Content-Type".to_string(), "application/sparql-query".to_string()),
            ],
            body: query,
        };

        let mut i = 0;
        loop {
            match self.client.send(&request).and_then(|body| parse_results(&body)) {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if i < self.http_retries {
                        tracing::warn!("sparql query failed, retrying: {e:#}");
                    } else {
                        tracing::error!("sparql query failed, exhausted retries: {e:#}");
                        return Err(e);
                    }
                    (self.sleep)(self.http_retry_wait);
                }
            }
            i += 1;
        }
    }

    /// Writes translations we already have to the output and skips fetching them again.
    pub fn load_existing(&mut self, old_mappings: &WikidataTranslations) -> anyhow::Result<()> {
        let already_have = old_mappings.get_all();
        if !already_have.is_empty() {
            tracing::info!("skipping {} mappings we already have", already_have.len());
            self.write_translations(already_have, old_mappings.get_update_times())?;
            self.visited.extend(already_have.keys().copied());
        }
        Ok(())
    }

    /// Flushes a batch of translations to disk.
    fn write_translations(&mut self, results: &TranslationMap, update_times: &HashMap<i64, i64>) -> anyhow::Result<()> {
        let update_time_default = now_millis();
        let mut ids: Vec<&i64> = results.keys().collect();
        ids.sort_unstable();
        for id in ids {
            let update_time = update_times.get(id).copied().unwrap_or(update_time_default);
            let line = serde_json::to_string(&(id.to_string(), &results[id], update_time))?;
            self.writer.write_all(line.as_bytes())?;
            self.writer.write_all(b"\n")?;
        }
        self.writer.flush()?;
        Ok(())
    }
}

/// Returns a filter that accepts elements `profile` wants wikidata translations for
/// ([`Profile::cares_about_wikidata_translation`](crate::profile::Profile::cares_about_wikidata_translation)).
pub fn profile_filter(
    profile: std::sync::Arc<dyn crate::profile::Profile>,
) -> impl Fn(&OsmElement) -> bool + Send + Sync + 'static {
    move |elem| profile.cares_about_wikidata_translation(elem)
}

/// Java `Wikidata.fetch(infile, outfile, config, profile, stats)`: [`fetch`] filtered by [`profile_filter`].
pub fn fetch_for_profile(
    infile: &OsmInputFile,
    outfile: &Path,
    config: &PlanetilerConfig,
    profile: std::sync::Arc<dyn crate::profile::Profile>,
    stats: StatsRef,
) -> anyhow::Result<()> {
    fetch(infile, outfile, config, profile_filter(profile), stats)
}

/// Loads any existing translations from `outfile`, then downloads translations for any wikidata element in `infile`
/// (that `filter` accepts) that have not already been downloaded and writes the results to `outfile`.
pub fn fetch(
    infile: &OsmInputFile,
    outfile: &Path,
    config: &PlanetilerConfig,
    filter: impl Fn(&OsmElement) -> bool + Send + Sync + 'static,
    stats: StatsRef,
) -> anyhow::Result<()> {
    fetch_with_max_age(infile, outfile, config, filter, stats, Duration::ZERO, 0)
}

/// Like [`fetch`], but re-fetches up to `update_limit` (0 = unlimited) translations older than `max_age`
/// (`Duration::ZERO` = never re-fetch).
pub fn fetch_with_max_age(
    infile: &OsmInputFile,
    outfile: &Path,
    config: &PlanetilerConfig,
    filter: impl Fn(&OsmElement) -> bool + Send + Sync + 'static,
    stats: StatsRef,
    max_age: Duration,
    update_limit: usize,
) -> anyhow::Result<()> {
    let client = Box::new(UreqWikidataClient::new(config));
    fetch_with_client(infile, outfile, config, filter, stats, max_age, update_limit, client)
}

/// Like [`fetch_with_max_age`] but with a custom HTTP `client`.
#[allow(clippy::too_many_arguments)]
pub fn fetch_with_client(
    infile: &OsmInputFile,
    outfile: &Path,
    config: &PlanetilerConfig,
    filter: impl Fn(&OsmElement) -> bool + Send + Sync + 'static,
    stats: StatsRef,
    max_age: Duration,
    update_limit: usize,
    client: Box<dyn WikidataClient>,
) -> anyhow::Result<()> {
    let timer = stats.start_stage("wikidata");
    let process_threads = config.threads.saturating_sub(1).max(1);
    tracing::info!("Starting with {process_threads} process threads");

    let old_mappings = load_with_max_age(outfile, max_age, update_limit);
    let writer = BufWriter::new(File::create(outfile).with_context(|| format!("{outfile:?}"))?);
    let source = Arc::new(infile.get()?);
    let mut fetcher = Wikidata::new(writer, client, 5_000, config);
    fetcher.load_existing(&old_mappings)?;
    drop(old_mappings);
    let counters = fetcher.counters.clone();
    let fetcher = Mutex::new(fetcher);

    let filter_counters = counters.clone();
    let pipeline = WorkerPipeline::start("wikidata", stats.clone())
        .from_generator("pbf", move |next| {
            source.for_each_block(&mut |block| {
                next.accept(block);
                Ok(())
            })
        })
        .add_buffer("pbf_blocks", process_threads * 2)
        .add_worker("filter", process_threads, move |prev, next| {
            // Only pass elements that the profile cares about to next step in pipeline.
            for block in prev {
                let (mut nodes, mut ways, mut rels) = (0, 0, 0);
                for elem in block.decode_elements()? {
                    match &elem {
                        OsmElement::Node(_) => nodes += 1,
                        OsmElement::Way(_) => ways += 1,
                        OsmElement::Relation(_) => rels += 1,
                        OsmElement::Other(_) => {}
                    }
                    if let Some(wikidata) = elem.get_string("wikidata")
                        && filter(&elem)
                    {
                        let qid = parse_qid(&wikidata);
                        if qid > 0 {
                            next.accept(qid);
                        }
                    }
                }
                filter_counters.blocks.fetch_add(1, Ordering::Relaxed);
                filter_counters.nodes.fetch_add(nodes, Ordering::Relaxed);
                filter_counters.ways.fetch_add(ways, Ordering::Relaxed);
                filter_counters.rels.fetch_add(rels, Ordering::Relaxed);
            }
            Ok(())
        })
        .add_buffer_batched("fetch_queue", 1_000_000, 100)
        .sink_to("fetch", 1, move |prev| {
            let mut fetcher = fetcher.lock();
            for id in prev {
                fetcher.fetch(id)?;
            }
            fetcher.flush()
        });

    let counter = |f: fn(&Counters) -> &AtomicI64| {
        let counters = counters.clone();
        move || f(&counters).load(Ordering::Relaxed)
    };
    let loggers = ProgressLoggers::create()
        .add_rate_counter("blocks", counter(|c| &c.blocks))
        .add_rate_counter_colored("nodes", counter(|c| &c.nodes), true)
        .add_rate_counter_colored("ways", counter(|c| &c.ways), true)
        .add_rate_counter_colored("rels", counter(|c| &c.rels), true)
        .add_rate_counter("wiki", counter(|c| &c.wikidatas))
        .add_file_size_path(outfile.to_path_buf())
        .new_line()
        .add_process_stats()
        .new_line()
        .add_pipeline_stats(&pipeline);

    pipeline.await_and_log(&loggers, config.log_interval)?;
    tracing::info!("DONE fetched: {}", counters.wikidatas.load(Ordering::Relaxed));
    timer.stop();
    Ok(())
}

/// Returns translations parsed from `path` that was written by a previous run of the downloader.
pub fn load(path: &Path) -> WikidataTranslations {
    let mut result = load_with_max_age(path, Duration::ZERO, 0);
    result.clear_update_times();
    result
}

fn load_with_max_age(path: &Path, max_age: Duration, update_limit: usize) -> WikidataTranslations {
    let start = Instant::now();
    if !path.exists() {
        tracing::info!("no wikidata translations found, run with --fetch-wikidata to download");
        return WikidataTranslations::default();
    }
    let result = File::open(path)
        .map_err(anyhow::Error::from)
        .and_then(|f| load_from_reader(BufReader::new(f), max_age, update_limit, now_millis()));
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match result {
        Ok(result) => {
            tracing::info!(
                "loaded from {} mappings from {} in {}",
                result.get_all().len(),
                abs.display(),
                format::duration(start.elapsed())
            );
            result
        }
        Err(e) => {
            tracing::info!("error loading {}: {e:#}", abs.display());
            WikidataTranslations::default()
        }
    }
}

/// Returns translations parsed from `reader` where each line is a JSON array where first element is the ID, second
/// element is a map from language to translation, and the optional third element is the update time in epoch millis.
///
/// Entries updated before `now_millis - max_age` are dropped (so they get re-fetched), up to `update_limit` of them
/// (0 = unlimited). `max_age` of zero keeps everything.
pub fn load_from_reader(
    reader: impl BufRead,
    max_age: Duration,
    update_limit: usize,
    now_millis: i64,
) -> anyhow::Result<WikidataTranslations> {
    let mut mappings = WikidataTranslations::default();
    let update_time_limit = (!max_age.is_zero()).then(|| now_millis - max_age.as_millis() as i64);
    let mut update_counter = 0;
    for line in reader.lines() {
        let line = line?;
        let node: Json = serde_json::from_str(&line)?;
        let id: i64 = match node.get(0) {
            Some(Json::String(s)) => s.parse()?,
            Some(Json::Number(n)) => n.as_i64().context("bad id")?,
            other => bail!("Invalid wikidata id: {other:?}"),
        };
        let update_time = node.get(2).and_then(Json::as_i64).unwrap_or(0);
        if let Some(limit) = update_time_limit
            && update_time < limit
            && (update_limit == 0 || update_counter < update_limit)
        {
            // do not load old entries => new translations will be fetched later
            update_counter += 1;
            continue;
        }
        mappings.put_update_time(id, update_time);
        if let Some(Json::Object(these)) = node.get(1) {
            for (lang, value) in these {
                let value = match value {
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                };
                mappings.put(id, lang, &value);
            }
        }
    }
    if update_counter > 0 {
        tracing::info!("{update_counter} translations dropped as too old, will be re-fetched");
    }
    Ok(mappings)
}

/// In-memory wikidata translations: QID -> language -> name.
#[derive(Clone, Debug, Default)]
pub struct WikidataTranslations {
    data: TranslationMap,
    update_times: HashMap<i64, i64>,
}

impl WikidataTranslations {
    pub fn new() -> WikidataTranslations {
        WikidataTranslations::default()
    }

    /// Returns a map from language code to translated name for `qid`.
    pub fn get(&self, qid: i64) -> Option<&BTreeMap<String, String>> {
        self.data.get(&qid)
    }

    pub fn clear_update_times(&mut self) {
        self.update_times.clear();
    }

    /// Returns all maps from language code to translated name by QID.
    pub fn get_all(&self) -> &TranslationMap {
        &self.data
    }

    /// Returns update times (epoch millis) by QID.
    pub fn get_update_times(&self) -> &HashMap<i64, i64> {
        &self.update_times
    }

    /// Stores an update time (epoch millis) for `qid`.
    pub fn put_update_time(&mut self, qid: i64, update_time_millis: i64) {
        self.update_times.insert(qid, update_time_millis);
    }

    /// Stores a name translation for `qid` in `lang`.
    pub fn put(&mut self, qid: i64, lang: &str, value: &str) {
        self.data.entry(qid).or_default().insert(lang.to_string(), value.to_string());
    }
}

impl TranslationProvider for WikidataTranslations {
    fn get_name_translations(&self, tags: &Tags) -> Option<Cow<'_, BTreeMap<String, String>>> {
        let wikidata_id = match tags.get("wikidata") {
            Some(Value::String(s)) => parse_qid(s),
            _ => 0,
        };
        if wikidata_id > 0 { self.get(wikidata_id).map(Cow::Borrowed) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::config::arguments::Arguments;

    const RESPONSE: &str = r#"
    {
      "head" : {
        "vars" : [ "id", "label" ]
      },
      "results" : {
        "bindings" : [ {
          "id" : {
            "type" : "uri",
            "value" : "http://www.wikidata.org/entity/Q1"
          },
          "label" : {
            "xml:lang" : "en",
            "type" : "literal",
            "value" : "en name"
          }
        }, {
          "id" : {
            "type" : "uri",
            "value" : "http://www.wikidata.org/entity/Q1"
          },
          "label" : {
            "xml:lang" : "es",
            "type" : "literal",
            "value" : "es name"
          }
        }, {
          "id" : {
            "type" : "uri",
            "value" : "http://www.wikidata.org/entity/Q2"
          },
          "label" : {
            "xml:lang" : "es",
            "type" : "literal",
            "value" : "es name2"
          }
        } ]
      }
    }
    "#;

    const WIKIDATA_NAMES_LEGACY_JSON: &str = r#"["1",{"en":"English 1","de":"Deutch 1"}]
["2",{"en":"English 2","de":"Deutch 2"}]
["3",{"en":"English 3","de":"Deutch 3"}]
"#;

    const CLOCK_MILLIS: i64 = 60_000;

    const WIKIDATA_NAMES_JSON: &str = r#"["1",{"en":"English 1","de":"Deutch 1"},55000]
["2",{"en":"English 2","de":"Deutch 2"},30000]
["3",{"en":"English 3","de":"Deutch 3"},30000]
"#;

    fn config() -> PlanetilerConfig {
        PlanetilerConfig::from(&Arguments::from_args(&["--http-retries=1"])).unwrap()
    }

    /// Mock client that records requests and pops responses from a queue (`None` = fail with an IO error).
    #[derive(Clone, Default)]
    struct MockClient {
        requests: Arc<Mutex<Vec<WikidataRequest>>>,
        responses: Arc<Mutex<VecDeque<Option<String>>>>,
    }

    impl MockClient {
        fn respond(&self, response: Option<&str>) {
            self.responses.lock().push_back(response.map(String::from));
        }
    }

    impl WikidataClient for MockClient {
        fn send(&self, request: &WikidataRequest) -> anyhow::Result<Vec<u8>> {
            self.requests.lock().push(request.clone());
            match self.responses.lock().pop_front() {
                Some(Some(body)) => Ok(body.into_bytes()),
                Some(None) => Err(std::io::Error::other("failed").into()),
                None => panic!("unexpected request"),
            }
        }
    }

    fn create_fixture(client: &MockClient, batch_size: usize) -> Wikidata<Vec<u8>> {
        let mut result = Wikidata::new(Vec::new(), Box::new(client.clone()), batch_size, &config());
        // don't sleep in tests
        result.sleep = |_| {};
        result
    }

    fn load_str(s: &str) -> WikidataTranslations {
        load_from_reader(s.as_bytes(), Duration::ZERO, 0, now_millis()).unwrap()
    }

    fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn test_wikidata_translations() {
        let expected = map(&[("en", "en value"), ("es", "es value")]);
        let mut translations = WikidataTranslations::new();
        assert_eq!(None, translations.get(1));
        translations.put(1, "en", "en value");
        translations.put(1, "es", "es value");
        assert_eq!(Some(&expected), translations.get(1));
        let mut elem = Tags::default();
        assert_eq!(None, translations.get_name_translations(&elem));
        elem.insert("wikidata".into(), Value::str("Qgarbage"));
        assert_eq!(None, translations.get_name_translations(&elem));
        elem.insert("wikidata".into(), Value::str("Q1"));
        assert_eq!(Some(&expected), translations.get_name_translations(&elem).as_deref());
    }

    #[test]
    fn test_fetch_wikidata() {
        let client = MockClient::default();
        let mut fixture = create_fixture(&client, 2);
        fixture.fetch(1).unwrap();
        assert!(client.requests.lock().is_empty());
        client.respond(Some(RESPONSE));
        fixture.fetch(2).unwrap();

        // verify http response
        {
            let requests = client.requests.lock();
            assert_eq!(1, requests.len());
            let request = &requests[0];
            assert_eq!("https://query.wikidata.org/bigdata/namespace/wdq/sparql", request.url);
            assert_eq!("POST", request.method);
            assert_eq!(Some("application/sparql-results+json"), request.header("Accept"));
            assert_eq!(Some("application/sparql-query"), request.header("Content-Type"));
            assert_eq!(
                "SELECT ?id ?label where { VALUES ?id { wd:Q1 wd:Q2 } ?id (owl:sameAs* / rdfs:label) ?label }",
                request.body
            );
        }

        // can load serialized data
        let written = String::from_utf8(fixture.writer.clone()).unwrap();
        let translations = load_str(&written);
        assert_eq!(Some(&map(&[("en", "en name"), ("es", "es name")])), translations.get(1));
        assert_eq!(Some(&map(&[("es", "es name2")])), translations.get(2));

        // do not re-request on subsequent loads
        let client2 = MockClient::default();
        let mut fixture2 = create_fixture(&client2, 2);
        fixture2.load_existing(&load_str(&written)).unwrap();
        fixture2.fetch(1).unwrap();
        fixture2.fetch(2).unwrap();
        fixture2.fetch(1).unwrap();
        fixture2.flush().unwrap();
        assert!(client2.requests.lock().is_empty());
    }

    #[test]
    fn test_retry_failed_request_once() {
        let client = MockClient::default();
        let mut fixture = create_fixture(&client, 1);
        // fail once then succeed
        client.respond(None);
        client.respond(Some(RESPONSE));
        fixture.fetch(1).unwrap();
        let translations = load_str(&String::from_utf8(fixture.writer.clone()).unwrap());
        assert_eq!(Some(&map(&[("en", "en name"), ("es", "es name")])), translations.get(1));
        assert_eq!(Some(&map(&[("es", "es name2")])), translations.get(2));

        // fail all subsequent requests
        client.respond(None);
        client.respond(None);
        let err = fixture.fetch(2).unwrap_err();
        assert!(err.chain().any(|e| e.is::<std::io::Error>()), "{err:?}");
        assert_eq!(4, client.requests.lock().len());
    }

    #[test]
    fn test_legacy_wikidata_names_json() {
        // no timestamp + age limit set => all old => all should be dropped
        let translations =
            load_from_reader(WIKIDATA_NAMES_LEGACY_JSON.as_bytes(), Duration::from_secs(1), 0, CLOCK_MILLIS).unwrap();
        assert_eq!(0, translations.get_all().len());
    }

    #[test]
    fn test_wikidata_names_json_max_age() {
        // 10s => item 1 is 5s old hence fresh, the rest is 30s old hence outdated
        let translations =
            load_from_reader(WIKIDATA_NAMES_JSON.as_bytes(), Duration::from_secs(10), 0, CLOCK_MILLIS).unwrap();
        assert_eq!(1, translations.get_all().len());
    }

    #[test]
    fn test_wikidata_names_json_update_limit() {
        let translations =
            load_from_reader(WIKIDATA_NAMES_JSON.as_bytes(), Duration::from_secs(1), 1, CLOCK_MILLIS).unwrap();
        assert_eq!(2, translations.get_all().len());
    }

    #[test]
    fn test_parse_qid() {
        assert_eq!(123, parse_qid("Q123"));
        assert_eq!(0, parse_qid("Qgarbage"));
        assert_eq!(0, parse_qid("123"));
        assert!(extract_id_from_wikidata_iri("https://example.com/Q1").is_err());
    }

    #[test]
    fn test_fetch_end_to_end_with_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let outfile = dir.path().join("wikidata.json");
        std::fs::write(&outfile, "[\"5\",{\"en\":\"five\"},1000]\n").unwrap();

        let input = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/monaco-latest.osm.pbf");
        let client = MockClient::default();
        client.respond(Some(RESPONSE));
        let seen = Arc::new(AtomicI64::new(0));
        let seen2 = seen.clone();
        fetch_with_client(
            &OsmInputFile::new(input, false),
            &outfile,
            &PlanetilerConfig::defaults(),
            move |elem| {
                seen2.fetch_add(1, Ordering::Relaxed);
                elem.id() < 0
            },
            crate::stats::stats::InMemoryStats::arc(),
            Duration::ZERO,
            0,
            Box::new(client.clone()),
        )
        .unwrap();
        assert!(seen.load(Ordering::Relaxed) > 0);
        // filter rejected everything, so no requests were made but existing translations were kept
        assert!(client.requests.lock().is_empty());
        let loaded = load(&outfile);
        assert_eq!(Some(&map(&[("en", "five")])), loaded.get(5));
        assert!(loaded.get_update_times().is_empty());
    }
}
