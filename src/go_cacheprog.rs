// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! An external build cache for the go command (`GOCACHEPROG`).
//!
//! The go command computes the cache keys itself and only asks for blobs to be
//! stored and fetched, so nothing here knows about Go compiler arguments. The
//! protocol is described in `go doc cmd/go/internal/cacheprog`.

use crate::cache::{Cache, CacheWrite, Storage};
use crate::errors::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

const META_OBJECT: &str = "meta";
const BODY_OBJECT: &str = "body";

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "Command")]
    command: String,
    #[serde(rename = "ActionID", default)]
    action_id: String,
    #[serde(rename = "OutputID", default)]
    output_id: String,
    #[serde(rename = "BodySize", default)]
    body_size: u64,
}

#[derive(Debug, Default, Serialize)]
struct Response {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "Err", skip_serializing_if = "Option::is_none")]
    err: Option<String>,
    #[serde(rename = "KnownCommands", skip_serializing_if = "Option::is_none")]
    known_commands: Option<&'static [&'static str]>,
    #[serde(rename = "Miss", skip_serializing_if = "std::ops::Not::not")]
    miss: bool,
    #[serde(rename = "OutputID", skip_serializing_if = "Option::is_none")]
    output_id: Option<String>,
    #[serde(rename = "Size", skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(rename = "DiskPath", skip_serializing_if = "Option::is_none")]
    disk_path: Option<PathBuf>,
}

/// What is stored next to the body of an entry.
#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    output_id: String,
    size: u64,
}

/// Counters for one run of the cache program.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub read_errors: u64,
    pub writes: u64,
    pub write_errors: u64,
    pub read_hit_duration: Duration,
    pub write_duration: Duration,
}

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    read_errors: AtomicU64,
    writes: AtomicU64,
    write_errors: AtomicU64,
    read_hit_micros: AtomicU64,
    write_micros: AtomicU64,
}

struct Shared<W> {
    out: Mutex<W>,
    dir: PathBuf,
    read_only: bool,
    runtime: Handle,
    counters: Counters,
}

/// The storage key for a go action ID. Go action IDs are hashed again so they
/// cannot collide with the keys of compiler cache entries.
fn storage_key(action_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sccache-go-cacheprog-v1\0");
    hasher.update(action_id.as_bytes());
    hasher.finalize().to_hex().to_string()
}

impl<W: Write> Shared<W> {
    fn send(&self, response: &Response) -> Result<()> {
        let mut out = self.out.lock().unwrap();
        serde_json::to_writer(&mut *out, response)?;
        out.write_all(b"\n")?;
        out.flush()?;
        Ok(())
    }

    /// Go reads entries from files it is handed the path of, so every hit and
    /// every put has to end up as a plain file in `dir`.
    fn stage(
        &self,
        key: &str,
        write: impl FnOnce(&mut BufWriter<&File>) -> Result<()>,
    ) -> Result<PathBuf> {
        let tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        let mut writer = BufWriter::new(tmp.as_file());
        write(&mut writer)?;
        writer.flush()?;
        drop(writer);
        let path = self.dir.join(key);
        // Replacing a file go still has open fails on Windows; the one
        // already there holds the same entry.
        if let Err(e) = tmp.persist(&path)
            && !path.exists()
        {
            return Err(e.into());
        }
        Ok(path)
    }

    fn fetch(&self, storage: &dyn Storage, key: &str) -> Result<Option<(Meta, PathBuf)>> {
        let Cache::Hit(mut entry) = self.runtime.block_on(storage.get(key))? else {
            return Ok(None);
        };
        let mut meta = vec![];
        entry.get_object(META_OBJECT, &mut meta)?;
        let meta: Meta = serde_json::from_slice(&meta)?;
        let path = self.stage(key, |file| {
            entry.get_object(BODY_OBJECT, file)?;
            Ok(())
        })?;
        let size = path.metadata()?.len();
        if size != meta.size {
            bail!("entry {key} has {size} bytes, expected {}", meta.size);
        }
        Ok(Some((meta, path)))
    }

    fn get(&self, storage: &dyn Storage, request: &Request) -> Result<()> {
        let key = storage_key(&request.action_id);
        let start = Instant::now();
        let response = match self.fetch(storage, &key) {
            Ok(Some((meta, path))) => {
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .read_hit_micros
                    .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
                Response {
                    id: request.id,
                    output_id: Some(meta.output_id),
                    size: Some(meta.size),
                    disk_path: Some(path),
                    ..Default::default()
                }
            }
            Ok(None) => {
                self.counters.misses.fetch_add(1, Ordering::Relaxed);
                Response {
                    id: request.id,
                    miss: true,
                    ..Default::default()
                }
            }
            // A cache that cannot be read must not fail the build.
            Err(e) => {
                debug!("go cacheprog: get {key} failed: {e:#}");
                self.counters.read_errors.fetch_add(1, Ordering::Relaxed);
                Response {
                    id: request.id,
                    miss: true,
                    ..Default::default()
                }
            }
        };
        self.send(&response)
    }

    fn store(&self, storage: &dyn Storage, key: &str, meta: &Meta, body: &[u8]) -> Result<()> {
        let mut entry = CacheWrite::new();
        entry.put_object(META_OBJECT, &mut &serde_json::to_vec(meta)?[..], None)?;
        entry.put_object(BODY_OBJECT, &mut &body[..], None)?;
        self.runtime.block_on(storage.put(key, entry))?;
        Ok(())
    }

    fn put(&self, storage: &dyn Storage, request: &Request, body: &[u8]) -> Result<()> {
        let key = storage_key(&request.action_id);
        let response = match self.stage(&key, |file| Ok(file.write_all(body)?)) {
            Ok(path) => Response {
                id: request.id,
                disk_path: Some(path),
                ..Default::default()
            },
            Err(e) => Response {
                id: request.id,
                err: Some(format!("{e:#}")),
                ..Default::default()
            },
        };
        // Answer before uploading: go only needs the local file to carry on.
        self.send(&response)?;
        if self.read_only || response.err.is_some() {
            return Ok(());
        }
        let meta = Meta {
            output_id: request.output_id.clone(),
            size: body.len() as u64,
        };
        let start = Instant::now();
        match self.store(storage, &key, &meta, body) {
            Ok(()) => {
                self.counters
                    .write_micros
                    .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
                self.counters.writes.fetch_add(1, Ordering::Relaxed)
            }
            Err(e) => {
                debug!("go cacheprog: put {key} failed: {e:#}");
                self.counters.write_errors.fetch_add(1, Ordering::Relaxed)
            }
        };
        Ok(())
    }
}

fn read_line(input: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<bool> {
    loop {
        line.clear();
        if input.read_until(b'\n', line)? == 0 {
            return Ok(false);
        }
        while line.last().is_some_and(|b| b.is_ascii_whitespace()) {
            line.pop();
        }
        if !line.is_empty() {
            return Ok(true);
        }
    }
}

/// Serve the go command on `input`/`output` until it sends `close` or hangs up.
///
/// Requests are handled concurrently, one thread per element of `storages`.
/// Entries handed to go are staged in `dir`, which must outlive the go command.
pub fn serve<R, W>(
    mut input: R,
    output: W,
    storages: Vec<Arc<dyn Storage>>,
    runtime: &Handle,
    read_only: bool,
    dir: &Path,
) -> Result<Stats>
where
    R: BufRead,
    W: Write + Send,
{
    let shared = Shared {
        out: Mutex::new(output),
        dir: dir.to_owned(),
        read_only,
        runtime: runtime.clone(),
        counters: Counters::default(),
    };
    shared.send(&Response {
        known_commands: Some(&["get", "put", "close"]),
        ..Default::default()
    })?;

    let (tx, rx) = mpsc::channel::<(Request, Vec<u8>)>();
    let rx = Mutex::new(rx);
    let close_id = thread::scope(|scope| -> Result<Option<i64>> {
        for storage in &storages {
            let (shared, rx) = (&shared, &rx);
            scope.spawn(move || {
                loop {
                    let job = rx.lock().unwrap().recv();
                    let Ok((request, body)) = job else { break };
                    let result = match request.command.as_str() {
                        "get" => shared.get(storage.as_ref(), &request),
                        "put" => shared.put(storage.as_ref(), &request, &body),
                        other => shared.send(&Response {
                            id: request.id,
                            err: Some(format!("unknown command {other:?}")),
                            ..Default::default()
                        }),
                    };
                    if let Err(e) = result {
                        debug!("go cacheprog: cannot answer request: {e:#}");
                    }
                }
            });
        }
        // Dropping the sender at the end of this closure lets the workers
        // drain the queue and exit; the scope then waits for their uploads.
        let tx = tx;
        let mut line = vec![];
        while read_line(&mut input, &mut line)? {
            let request: Request =
                serde_json::from_slice(&line).context("malformed GOCACHEPROG request")?;
            if request.command == "close" {
                return Ok(Some(request.id));
            }
            let mut body = vec![];
            if request.command == "put" && request.body_size > 0 {
                if !read_line(&mut input, &mut line)? {
                    bail!("GOCACHEPROG request ended before its body");
                }
                let encoded = line
                    .strip_prefix(b"\"")
                    .and_then(|l| l.strip_suffix(b"\""))
                    .context("malformed GOCACHEPROG body")?;
                body = BASE64
                    .decode(encoded)
                    .context("malformed GOCACHEPROG body")?;
                if body.len() as u64 != request.body_size {
                    bail!(
                        "GOCACHEPROG body has {} bytes, expected {}",
                        body.len(),
                        request.body_size
                    );
                }
            }
            tx.send((request, body)).ok();
        }
        Ok(None)
    })?;
    if let Some(id) = close_id {
        shared.send(&Response {
            id,
            ..Default::default()
        })?;
    }

    let c = &shared.counters;
    Ok(Stats {
        hits: c.hits.load(Ordering::Relaxed),
        misses: c.misses.load(Ordering::Relaxed),
        read_errors: c.read_errors.load(Ordering::Relaxed),
        writes: c.writes.load(Ordering::Relaxed),
        write_errors: c.write_errors.load(Ordering::Relaxed),
        read_hit_duration: Duration::from_micros(c.read_hit_micros.load(Ordering::Relaxed)),
        write_duration: Duration::from_micros(c.write_micros.load(Ordering::Relaxed)),
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cache::CacheMode;
    use crate::cache::disk::DiskCache;
    use crate::config::PreprocessorCacheModeConfig;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::collections::HashMap;

    struct Fixture {
        runtime: tokio::runtime::Runtime,
        cache_dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                runtime: tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .build()
                    .unwrap(),
                cache_dir: tempfile::tempdir().unwrap(),
            }
        }

        fn disk(&self) -> Arc<dyn Storage> {
            Arc::new(DiskCache::new(
                self.cache_dir.path(),
                u64::MAX,
                self.runtime.handle(),
                PreprocessorCacheModeConfig::default(),
                CacheMode::ReadWrite,
                vec![],
            ))
        }

        /// One run of the cache program, as one go command would see it:
        /// responses by request ID, the stats, and the staging directory.
        fn run(
            &self,
            storage: Arc<dyn Storage>,
            read_only: bool,
            input: &str,
        ) -> Result<(HashMap<i64, Value>, Stats, tempfile::TempDir)> {
            let staging = tempfile::tempdir().unwrap();
            let mut output = vec![];
            let stats = serve(
                input.as_bytes(),
                &mut output,
                vec![storage.clone(), storage],
                self.runtime.handle(),
                read_only,
                staging.path(),
            )?;
            let mut lines = output.split(|b| *b == b'\n').filter(|l| !l.is_empty());
            let hello: Value = serde_json::from_slice(lines.next().unwrap()).unwrap();
            assert_eq!(hello["ID"], 0);
            assert_eq!(
                hello["KnownCommands"],
                serde_json::json!(["get", "put", "close"])
            );
            let responses = lines
                .map(|l| serde_json::from_slice::<Value>(l).unwrap())
                .map(|v| (v["ID"].as_i64().unwrap(), v))
                .collect();
            Ok((responses, stats, staging))
        }
    }

    fn id(seed: u8) -> String {
        BASE64.encode([seed; 32])
    }

    fn put(request: i64, action: u8, output: u8, body: &[u8]) -> String {
        let mut line = format!(
            r#"{{"ID":{request},"Command":"put","ActionID":"{}","OutputID":"{}","BodySize":{}}}"#,
            id(action),
            id(output),
            body.len()
        );
        line.push('\n');
        if !body.is_empty() {
            line += &format!("\"{}\"\n", BASE64.encode(body));
        }
        line
    }

    fn get(request: i64, action: u8) -> String {
        format!(
            "{{\"ID\":{request},\"Command\":\"get\",\"ActionID\":\"{}\"}}\n",
            id(action)
        )
    }

    fn close(request: i64) -> String {
        format!("{{\"ID\":{request},\"Command\":\"close\"}}\n")
    }

    fn body_at(response: &Value) -> Vec<u8> {
        std::fs::read(response["DiskPath"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn put_then_get_in_a_later_run() {
        let f = Fixture::new();
        let big = (0..300_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let input =
            put(1, 1, 101, b"first") + &put(2, 2, 102, &big) + &put(3, 3, 103, b"") + &close(4);
        let (responses, stats, staging) = f.run(f.disk(), false, &input).unwrap();
        assert_eq!(body_at(&responses[&1]), b"first");
        assert_eq!(body_at(&responses[&2]), big);
        assert_eq!(body_at(&responses[&3]), b"");
        assert_eq!(responses[&4], serde_json::json!({"ID": 4}));
        assert_eq!(stats.writes, 3);
        drop(staging);

        let input = get(1, 2) + &get(2, 1) + &get(3, 3) + &get(4, 9) + &close(5);
        let (responses, stats, _staging) = f.run(f.disk(), false, &input).unwrap();
        assert_eq!(responses[&1]["OutputID"], id(102));
        assert_eq!(responses[&1]["Size"], big.len());
        assert_eq!(body_at(&responses[&1]), big);
        assert_eq!(responses[&2]["OutputID"], id(101));
        assert_eq!(body_at(&responses[&2]), b"first");
        assert_eq!(responses[&3]["OutputID"], id(103));
        assert_eq!(responses[&3]["Size"], 0);
        assert_eq!(body_at(&responses[&3]), b"");
        assert_eq!(responses[&4], serde_json::json!({"ID": 4, "Miss": true}));
        assert_eq!((stats.hits, stats.misses, stats.read_errors), (3, 1, 0));
    }

    #[test]
    fn entries_do_not_use_the_action_id_as_key() {
        let f = Fixture::new();
        f.run(f.disk(), false, &(put(1, 1, 101, b"x") + &close(2)))
            .unwrap();
        let hex = "01".repeat(32);
        for key in [hex.as_str(), id(1).as_str()] {
            let found = f.runtime.block_on(f.disk().get(key));
            assert!(
                !matches!(found, Ok(Cache::Hit(_))),
                "entry stored under {key}"
            );
        }
    }

    #[test]
    fn read_only_put_is_staged_but_not_stored() {
        let f = Fixture::new();
        let (responses, stats, _staging) = f
            .run(f.disk(), true, &(put(1, 1, 101, b"body") + &close(2)))
            .unwrap();
        assert_eq!(body_at(&responses[&1]), b"body");
        assert_eq!(stats.writes, 0);

        let (responses, ..) = f.run(f.disk(), true, &(get(1, 1) + &close(2))).unwrap();
        assert_eq!(responses[&1]["Miss"], true);
    }

    struct BrokenStorage;

    #[async_trait]
    impl Storage for BrokenStorage {
        async fn get(&self, _key: &str) -> Result<Cache> {
            bail!("backend is down")
        }
        async fn put(&self, _key: &str, _entry: CacheWrite) -> Result<Duration> {
            bail!("backend is down")
        }
        fn location(&self) -> String {
            String::new()
        }
        async fn current_size(&self) -> Result<Option<u64>> {
            Ok(None)
        }
        async fn max_size(&self) -> Result<Option<u64>> {
            Ok(None)
        }
    }

    #[test]
    fn a_failing_backend_does_not_fail_the_build() {
        let f = Fixture::new();
        let input = put(1, 1, 101, b"body") + &get(2, 2) + &close(3);
        let (responses, stats, _staging) = f.run(Arc::new(BrokenStorage), false, &input).unwrap();
        assert_eq!(body_at(&responses[&1]), b"body");
        assert!(responses[&1].get("Err").is_none());
        assert_eq!(responses[&2], serde_json::json!({"ID": 2, "Miss": true}));
        assert_eq!((stats.write_errors, stats.read_errors), (1, 1));
        assert_eq!((stats.writes, stats.hits, stats.misses), (0, 0, 0));
    }

    #[test]
    fn a_corrupt_entry_is_a_miss() {
        let f = Fixture::new();
        let mut entry = CacheWrite::new();
        let meta = Meta {
            output_id: id(101),
            size: 99,
        };
        entry
            .put_object(
                META_OBJECT,
                &mut &serde_json::to_vec(&meta).unwrap()[..],
                None,
            )
            .unwrap();
        entry
            .put_object(BODY_OBJECT, &mut &b"short"[..], None)
            .unwrap();
        f.runtime
            .block_on(f.disk().put(&storage_key(&id(1)), entry))
            .unwrap();

        let (responses, stats, _staging) =
            f.run(f.disk(), false, &(get(1, 1) + &close(2))).unwrap();
        assert_eq!(responses[&1], serde_json::json!({"ID": 1, "Miss": true}));
        assert_eq!(stats.read_errors, 1);
    }

    #[test]
    fn unknown_command_gets_an_error_response() {
        let f = Fixture::new();
        let input = "{\"ID\":1,\"Command\":\"get2\"}\n".to_owned() + &close(2);
        let (responses, ..) = f.run(f.disk(), false, &input).unwrap();
        assert!(responses[&1]["Err"].as_str().unwrap().contains("get2"));
        assert_eq!(responses[&2], serde_json::json!({"ID": 2}));
    }

    #[test]
    fn end_of_input_without_close_is_not_an_error() {
        let f = Fixture::new();
        let (responses, stats, _staging) =
            f.run(f.disk(), false, &put(1, 1, 101, b"body")).unwrap();
        assert_eq!(body_at(&responses[&1]), b"body");
        assert_eq!(stats.writes, 1);
    }

    #[test]
    fn body_shorter_than_announced_is_rejected() {
        let f = Fixture::new();
        let input = put(1, 1, 101, b"body").replace("\"BodySize\":4", "\"BodySize\":5");
        assert!(f.run(f.disk(), false, &input).is_err());
    }
}
