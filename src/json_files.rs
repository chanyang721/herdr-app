//! JSON file snapshots for mods. One process-wide worker owns the cache: file
//! metadata, reads, SHA-256, serde parsing and host-value conversion never run on
//! the caller's (possibly UI) thread. An unchanged caller hash returns no value.
use gpui_shell::{HostError, HostValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
const CACHE_ENTRIES: usize = 32;
const CACHE_BYTES: usize = 32 * 1024 * 1024;
const QUEUE: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    created: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl Stamp {
    fn of(metadata: &Metadata) -> Result<Self, String> {
        if !metadata.is_file() {
            return Err("JSON input is not a regular file".into());
        }
        Ok(Self {
            modified: metadata.modified().map_err(|e| e.to_string())?,
            created: metadata.created().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        })
    }
}

#[derive(Clone)]
enum Parsed {
    Ok(Arc<Value>),
    Error(String),
}

struct Entry {
    stamp: Stamp,
    hash: String,
    parsed: Parsed,
    weight: usize,
    used: u64,
}

/// The blocking cache is separate from worker transport so filesystem behavior
/// can be tested without an app, window or JavaScript VM. Arc only shares the
/// parsed tree; returning an unchanged hash never clones or materializes it.
pub(crate) struct ReadResult {
    pub status: &'static str,
    pub hash: Option<String>,
    pub changed: bool,
    pub value: Option<Arc<Value>>,
    pub error_kind: Option<&'static str>,
    pub error: Option<String>,
}

impl ReadResult {
    fn error(status: &'static str, message: impl Into<String>) -> Self {
        Self {
            status, hash: None, changed: false, value: None,
            error_kind: (status == "read_error").then_some("io"),
            error: Some(message.into()),
        }
    }

    fn io_error(error: std::io::Error) -> Self {
        let kind = if error.kind() == std::io::ErrorKind::NotFound { "not_found" } else { "io" };
        let mut result = Self::error("read_error", error.to_string());
        result.error_kind = Some(kind);
        result
    }

    fn from_entry(entry: &Entry, caller_hash: Option<&str>) -> Self {
        match &entry.parsed {
            Parsed::Ok(value) => {
                let changed = caller_hash != Some(entry.hash.as_str());
                Self {
                    status: "ok",
                    hash: Some(entry.hash.clone()),
                    changed,
                    value: changed.then(|| Arc::clone(value)),
                    error_kind: None,
                    error: None,
                }
            }
            Parsed::Error(message) => Self::error("parse_error", message.clone()),
        }
    }

    // Called only on the worker. The Future delivers an already materialized
    // HostValue; it does not recursively copy large serde trees on resumption.
    fn into_host(self) -> HostValue {
        let mut fields = vec![
            ("status".into(), HostValue::Str(self.status.into())),
            ("hash".into(), self.hash.map(HostValue::Str).unwrap_or(HostValue::Null)),
            ("changed".into(), HostValue::Bool(self.changed)),
            ("value".into(), self.value.as_deref().map(crate::mods::to_host).unwrap_or(HostValue::Null)),
        ];
        if let Some(kind) = self.error_kind {
            fields.push(("errorKind".into(), HostValue::Str(kind.into())));
        }
        // Raw OS messages may include paths or other machine details. Keep
        // diagnostics internal; callers receive only the neutral classification.
        drop(self.error);
        HostValue::Object(fields.into_iter().collect())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stats {
    reads: usize,
    hashes: usize,
    parses: usize,
    value_returns: usize,
}

pub(crate) struct Cache {
    entries: HashMap<PathBuf, Entry>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    clock: u64,
    #[cfg(test)]
    stats: Stats,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_limits(CACHE_ENTRIES, CACHE_BYTES)
    }
}

impl Cache {
    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(), bytes: 0, max_entries, max_bytes, clock: 0,
            #[cfg(test)]
            stats: Stats::default(),
        }
    }

    fn remove(&mut self, path: &Path) {
        if let Some(entry) = self.entries.remove(path) {
            self.bytes -= entry.weight;
        }
    }

    fn insert(&mut self, path: PathBuf, entry: Entry) {
        self.remove(&path);
        // A large individual result is still delivered to its caller, but does
        // not displace the bounded cache with an arbitrarily large parsed tree.
        if self.max_entries == 0 || entry.weight > self.max_bytes {
            return;
        }
        while self.entries.len() >= self.max_entries || self.bytes + entry.weight > self.max_bytes {
            let oldest = self.entries.iter().min_by_key(|(_, entry)| entry.used).map(|(path, _)| path.clone());
            let Some(oldest) = oldest else { break };
            self.remove(&oldest);
        }
        self.bytes += entry.weight;
        self.entries.insert(path, entry);
    }

    pub(crate) fn read(&mut self, path: &Path, caller_hash: Option<&str>, max_bytes: u64) -> Result<ReadResult, String> {
        self.read_observed(path, caller_hash, max_bytes, |_| {})
    }

    fn read_observed(
        &mut self,
        path: &Path,
        caller_hash: Option<&str>,
        max_bytes: u64,
        after_open: impl FnOnce(&File),
    ) -> Result<ReadResult, String> {
        // Validation lives here too; callers cannot bypass the public API's cap
        // by calling the blocking cache from another native integration.
        if max_bytes == 0 || max_bytes > MAX_BYTES {
            return Err(format!("maxBytes must be in 1..={MAX_BYTES}"));
        }
        let result = self.read_inner(path, caller_hash, max_bytes, after_open);
        #[cfg(test)]
        if result.value.is_some() {
            self.stats.value_returns += 1;
        }
        Ok(result)
    }

    fn read_inner(
        &mut self,
        path: &Path,
        caller_hash: Option<&str>,
        max_bytes: u64,
        after_open: impl FnOnce(&File),
    ) -> ReadResult {
        self.clock = self.clock.wrapping_add(1);
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.remove(path);
                return ReadResult::io_error(error);
            }
        };
        let stamp = match Stamp::of(&metadata) {
            Ok(stamp) => stamp,
            Err(message) => {
                self.remove(path);
                return ReadResult::error("read_error", message);
            }
        };
        if stamp.len > max_bytes {
            // This limit applies to every call, even if a different window
            // previously cached the file using a more generous limit.
            return ReadResult::error("too_large", format!("JSON file exceeds {max_bytes} bytes"));
        }
        if let Some(entry) = self.entries.get_mut(path).filter(|entry| entry.stamp == stamp) {
            entry.used = self.clock;
            return ReadResult::from_entry(entry, caller_hash);
        }

        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) => {
                self.remove(path);
                return ReadResult::io_error(error);
            }
        };
        if file.metadata().map_err(|e| e.to_string()).and_then(|metadata| Stamp::of(&metadata)).as_ref() != Ok(&stamp) {
            self.remove(path);
            return ReadResult::error("read_error", "JSON file changed before reading; retry");
        }
        after_open(&file);
        let mut bytes = Vec::with_capacity(stamp.len as usize);
        #[cfg(test)]
        { self.stats.reads += 1; }
        if let Err(error) = (&file).take(max_bytes + 1).read_to_end(&mut bytes) {
            self.remove(path);
            return ReadResult::error("read_error", error.to_string());
        }
        if bytes.len() as u64 > max_bytes {
            self.remove(path);
            return ReadResult::error("too_large", format!("JSON file exceeds {max_bytes} bytes"));
        }
        #[cfg(test)]
        { self.stats.hashes += 1; }
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let parsed = match self.entries.get(path).filter(|entry| entry.hash == hash) {
            // An mtime-only change still needs a bounded read and hash, but no
            // parse/deep copy if those bytes have already been parsed.
            Some(entry) => entry.parsed.clone(),
            None => {
                #[cfg(test)]
                { self.stats.parses += 1; }
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => Parsed::Ok(Arc::new(value)),
                    Err(error) => Parsed::Error(error.to_string()),
                }
            }
        };
        // Do not cache a snapshot read across an in-place write or atomic file
        // replacement. Compare the open handle AND current path after parsing.
        let stable = bytes.len() as u64 == stamp.len
            && file.metadata().ok().and_then(|metadata| Stamp::of(&metadata).ok()).as_ref() == Some(&stamp)
            && std::fs::metadata(path).ok().and_then(|metadata| Stamp::of(&metadata).ok()).as_ref() == Some(&stamp);
        if !stable {
            self.remove(path);
            return ReadResult::error("read_error", "JSON file changed while reading; retry");
        }
        let weight = std::mem::size_of::<Entry>() + path.as_os_str().len() * 2 + hash.capacity()
            + match &parsed { Parsed::Ok(value) => value_heap(value) + std::mem::size_of::<Value>(), Parsed::Error(message) => message.capacity() };
        let entry = Entry { stamp, hash, parsed, weight, used: self.clock };
        let result = ReadResult::from_entry(&entry, caller_hash);
        self.insert(path.to_owned(), entry);
        result
    }
}

// Account for the retained serde allocation, not just source length (a short
// array of small JSON values can occupy much more memory than its source).
fn value_heap(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity(),
        Value::Array(values) => values.capacity() * std::mem::size_of::<Value>() + values.iter().map(value_heap).sum::<usize>(),
        Value::Object(values) => values.iter().map(|(key, value)| {
            key.capacity() + std::mem::size_of::<(String, Value)>() + 4 * std::mem::size_of::<usize>() + value_heap(value)
        }).sum(),
        _ => 0,
    }
}

struct Job {
    path: String,
    hash: Option<String>,
    max_bytes: u64,
    reply: async_channel::Sender<Result<HostValue, String>>,
}

fn worker() -> Result<&'static async_channel::Sender<Job>, String> {
    static WORKER: OnceLock<Result<async_channel::Sender<Job>, String>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded::<Job>(QUEUE);
        std::thread::Builder::new().name("herdr-json-files".into()).spawn(move || {
            let mut cache = Cache::default();
            while let Ok(job) = receiver.recv_blocking() {
                if job.reply.is_closed() { continue; }
                let result = cache.read(Path::new(&job.path), job.hash.as_deref(), job.max_bytes).map(ReadResult::into_host);
                let _ = job.reply.send_blocking(result);
            }
        }).map_err(|e| e.to_string())?;
        Ok(sender)
    }).as_ref().map_err(Clone::clone)
}

/// Bounded queue backpressure suspends the future; it never blocks a UI thread
/// or starts a new native thread for each call/window.
pub fn read_json(
    path: String,
    hash: Option<String>,
    max_bytes: u64,
) -> impl std::future::Future<Output = Result<HostValue, HostError>> + Send {
    async move {
        let sender = worker().map_err(HostError::new)?;
        let (reply, result) = async_channel::bounded(1);
        sender.send(Job { path, hash, max_bytes, reply }).await.map_err(|_| HostError::new("JSON file worker stopped"))?;
        result.recv().await.map_err(|_| HostError::new("JSON file request was lost"))?.map_err(HostError::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{FileTimes, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!("herdr-json-{}-{}-{}", std::process::id(),
                SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }
    fn touch(path: &Path, prior: SystemTime) {
        OpenOptions::new().write(true).open(path).unwrap().set_times(FileTimes::new().set_modified(prior + Duration::from_secs(2))).unwrap();
    }
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        struct Wake(std::thread::Thread);
        impl std::task::Wake for Wake { fn wake(self: Arc<Self>) { self.0.unpark(); } }
        let waker = std::task::Waker::from(Arc::new(Wake(std::thread::current())));
        let mut context = std::task::Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(value) => return value,
                std::task::Poll::Pending => std::thread::park(),
            }
        }
    }

    #[test]
    fn hits_skip_read_hash_parse_and_unchanged_values_are_not_returned() {
        let temp = Temp::new();
        let path = temp.file("shared.json", r#"{"feed":[1,2,3]}"#);
        let mut cache = Cache::default();
        let first = cache.read(&path, None, DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(first.status, "ok");
        assert!(first.changed);
        let first_value = first.value.unwrap();
        let hash = first.hash.unwrap();
        let unchanged = cache.read(&path, Some(&hash), DEFAULT_MAX_BYTES).unwrap();
        assert!(!unchanged.changed);
        assert!(unchanged.value.is_none());
        let other_window = cache.read(&path, None, DEFAULT_MAX_BYTES).unwrap().value.unwrap();
        assert!(Arc::ptr_eq(&first_value, &other_window), "windows share the parsed allocation");
        assert_eq!(cache.stats, Stats { reads: 1, hashes: 1, parses: 1, value_returns: 2 });
    }

    #[test]
    fn changed_stamp_with_same_content_reuses_parse_and_different_bytes_change_hash() {
        let temp = Temp::new();
        let path = temp.file("touch.json", r#"{"a":1}"#);
        let mut cache = Cache::default();
        let first = cache.read(&path, None, DEFAULT_MAX_BYTES).unwrap();
        let prior = std::fs::metadata(&path).unwrap().modified().unwrap();
        touch(&path, prior);
        let same = cache.read(&path, first.hash.as_deref(), DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(same.status, "ok");
        assert!(!same.changed);
        assert!(same.value.is_none());
        assert_eq!(cache.stats.reads, 2);
        assert_eq!(cache.stats.hashes, 2);
        assert_eq!(cache.stats.parses, 1);
        std::fs::write(&path, r#"{"a":2}"#).unwrap();
        touch(&path, prior + Duration::from_secs(2));
        let changed = cache.read(&path, first.hash.as_deref(), DEFAULT_MAX_BYTES).unwrap();
        assert!(changed.changed);
        assert_ne!(first.hash, changed.hash);
        assert_eq!(changed.value.as_deref().unwrap()["a"], 2);
        assert_eq!(cache.stats.parses, 2);
    }

    #[test]
    fn errors_are_distinct_and_a_lower_per_call_limit_applies_to_hits() {
        let temp = Temp::new();
        let mut cache = Cache::default();
        let missing = cache.read(&temp.0.join("missing"), None, 1024).unwrap();
        assert_eq!(missing.status, "read_error");
        assert_eq!(missing.error_kind, Some("not_found"));
        assert!(missing.error.is_some());
        let host = missing.into_host();
        assert_eq!(host.get("errorKind").and_then(HostValue::as_str), Some("not_found"));
        assert!(host.get("error").is_none(), "OS diagnostics are not part of the public value");
        let directory = cache.read(&temp.0, None, 1024).unwrap();
        assert_eq!(directory.status, "read_error");
        assert_eq!(directory.error_kind, Some("io"));
        let host = directory.into_host();
        assert_eq!(host.get("errorKind").and_then(HostValue::as_str), Some("io"));
        assert!(host.get("error").is_none());
        let invalid = temp.file("invalid.json", "{ broken");
        let error = cache.read(&invalid, None, 1024).unwrap();
        assert_eq!(error.status, "parse_error");
        assert_eq!(error.error_kind, None);
        assert!(error.hash.is_none() && error.value.is_none() && !error.changed);
        let after_error = cache.stats;
        assert_eq!(cache.read(&invalid, None, 1024).unwrap().status, "parse_error");
        assert_eq!(cache.stats, after_error, "invalid unchanged JSON also skips read and parse");
        let host = cache.read(&invalid, None, 1024).unwrap().into_host();
        assert!(host.get("errorKind").is_none() && host.get("error").is_none());
        let invalid_utf8 = temp.0.join("invalid-utf8.json");
        std::fs::write(&invalid_utf8, [0xff, b'0']).unwrap();
        assert_eq!(cache.read(&invalid_utf8, None, 1024).unwrap().status, "parse_error");
        let exact = temp.file("exact-limit.json", "null");
        let null = cache.read(&exact, None, 4).unwrap();
        assert_eq!(null.status, "ok");
        assert!(null.changed);
        assert_eq!(null.value.as_deref(), Some(&Value::Null));
        let path = temp.file("valid.json", r#"{"feed":[1,2,3]}"#);
        let first = cache.read(&path, None, 1024).unwrap();
        let before = cache.stats;
        let too_large = cache.read(&path, first.hash.as_deref(), 4).unwrap();
        assert_eq!(too_large.status, "too_large");
        assert!(too_large.hash.is_none() && too_large.value.is_none() && !too_large.changed);
        assert_eq!(cache.stats, before);
        assert_eq!(cache.read(&path, first.hash.as_deref(), 1024).unwrap().status, "ok");
        assert!(cache.read(&path, None, 0).is_err());
        assert!(cache.read(&path, None, MAX_BYTES + 1).is_err());
    }

    #[test]
    fn growth_is_bounded_and_racing_writes_are_not_committed() {
        let temp = Temp::new();
        let path = temp.file("growing.json", "null");
        let mut cache = Cache::default();
        let growing = cache.read_observed(&path, None, 16, |_| {
            OpenOptions::new().append(true).open(&path).unwrap().write_all(&[b' '; 64]).unwrap();
        }).unwrap();
        assert_eq!(growing.status, "too_large");
        assert!(cache.entries.is_empty());
        assert_eq!(cache.stats.hashes, 0, "read cap rejects growth before hashing or parsing");
        std::fs::write(&path, "null").unwrap();
        let old = std::fs::metadata(&path).unwrap().modified().unwrap();
        let racing = cache.read_observed(&path, None, 1024, |_| {
            std::fs::write(&path, "true").unwrap();
            touch(&path, old);
        }).unwrap();
        assert_eq!(racing.status, "read_error");
        assert_eq!(racing.error_kind, Some("io"));
        assert!(cache.entries.is_empty());
        let fresh = cache.read(&path, None, 1024).unwrap();
        assert_eq!(fresh.status, "ok");
        assert_eq!(fresh.value.as_deref(), Some(&Value::Bool(true)));
    }

    #[test]
    fn cache_eviction_bounds_entries_and_estimated_parsed_heap() {
        let temp = Temp::new();
        let mut cache = Cache::with_limits(2, 4096);
        let a = temp.file("a.json", r#"{"a":1}"#);
        let b = temp.file("b.json", r#"{"b":2}"#);
        let c = temp.file("c.json", r#"{"c":3}"#);
        for path in [&a, &b, &a, &c] { cache.read(path, None, DEFAULT_MAX_BYTES).unwrap(); }
        assert!(cache.entries.contains_key(&a));
        assert!(!cache.entries.contains_key(&b), "least recently used entry is evicted");
        assert!(cache.entries.len() <= 2 && cache.bytes <= 4096);
        let large = temp.file("large.json", &format!("\"{}\"", "x".repeat(8192)));
        let result = cache.read(&large, None, DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(result.status, "ok");
        assert!(result.value.is_some());
        assert!(!cache.entries.contains_key(&large));
        assert!(cache.entries.len() <= 2 && cache.bytes <= 4096);
        std::fs::remove_file(&a).unwrap();
        assert_eq!(cache.read(&a, None, DEFAULT_MAX_BYTES).unwrap().status, "read_error");
        assert!(!cache.entries.contains_key(&a));
    }

    #[test]
    fn one_worker_is_shared_and_handles_concurrent_callers() {
        let temp = Temp::new();
        let path = temp.file("concurrent.json", r#"{"hello":"world"}"#);
        assert!(std::ptr::eq(worker().unwrap(), worker().unwrap()));
        let first = block_on(read_json(path.to_string_lossy().into_owned(), None, DEFAULT_MAX_BYTES)).unwrap();
        assert_eq!(first.get("status").and_then(HostValue::as_str), Some("ok"));
        let hash = first.get("hash").and_then(HostValue::as_str).unwrap().to_owned();
        let callers: Vec<_> = (0..8).map(|_| {
            let path = path.to_string_lossy().into_owned();
            let hash = hash.clone();
            std::thread::spawn(move || block_on(read_json(path, Some(hash), DEFAULT_MAX_BYTES)).unwrap())
        }).collect();
        for caller in callers {
            let value = caller.join().unwrap();
            assert!(matches!(value.get("changed"), Some(HostValue::Bool(false))));
            assert!(matches!(value.get("value"), Some(HostValue::Null)));
        }
    }

    #[test]
    #[ignore = "manual timing diagnostic; not a wall-clock CI threshold"]
    fn unchanged_and_large_json_timings() {
        let temp = Temp::new();
        let path = temp.file("large.json", &serde_json::to_string(&vec!["feed item"; 250_000]).unwrap());
        let mut cache = Cache::default();
        let started = Instant::now();
        let first = cache.read(&path, None, DEFAULT_MAX_BYTES).unwrap();
        let parse = started.elapsed();
        let started = Instant::now();
        for _ in 0..1000 {
            let result = cache.read(&path, first.hash.as_deref(), DEFAULT_MAX_BYTES).unwrap();
            assert!(result.value.is_none() && !result.changed);
        }
        eprintln!("json snapshot bytes={} first_read_hash_parse_us={} unchanged_1000_us={} reads={} hashes={} parses={}",
            std::fs::metadata(&path).unwrap().len(), parse.as_micros(), started.elapsed().as_micros(),
            cache.stats.reads, cache.stats.hashes, cache.stats.parses);
        assert_eq!(cache.stats.reads, 1);
        assert_eq!(cache.stats.parses, 1);
    }
}
