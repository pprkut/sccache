//! Tests for `sccache --go-cacheprog`.
//!
//! Any copyright is dedicated to the Public Domain.
//! http://creativecommons.org/publicdomain/zero/1.0/
pub mod helpers;

use anyhow::{Context, Result};
use assert_cmd::prelude::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fs_err as fs;
use helpers::{SCCACHE_BIN, stop_sccache};
use serde_json::Value;
use serial_test::serial;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const FRAME_LIMIT: usize = 64 * 1024;

struct Server {
    tempdir: tempfile::TempDir,
    frame_limit: Option<usize>,
}

impl Server {
    fn start(frame_limit: Option<usize>) -> Result<Self> {
        stop_sccache()?;
        let tempdir = tempfile::Builder::new()
            .prefix("sccache_test_go")
            .tempdir()?;
        let server = Server {
            tempdir,
            frame_limit,
        };
        server
            .sccache()
            .arg("--start-server")
            .assert()
            .try_success()
            .context("Failed to start sccache server")?;
        Ok(server)
    }

    fn sccache(&self) -> Command {
        let mut cmd = Command::new(SCCACHE_BIN.as_os_str());
        cmd.env("SCCACHE_DIR", self.tempdir.path().join("cache"));
        match self.frame_limit {
            Some(limit) => cmd.env("SCCACHE_MAX_FRAME_LENGTH", limit.to_string()),
            None => cmd.env_remove("SCCACHE_MAX_FRAME_LENGTH"),
        };
        cmd
    }

    fn stats(&self) -> Result<Value> {
        let output = self
            .sccache()
            .args(["--show-stats", "--stats-format=json"])
            .output()?;
        Ok(serde_json::from_slice::<Value>(&output.stdout)?["stats"].take())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        stop_sccache().expect("Stopping Sccache server failed");
    }
}

/// Plays the go command's side of the GOCACHEPROG protocol.
struct Go {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Go {
    fn spawn(server: &Server) -> Result<Self> {
        let mut child = server
            .sccache()
            .arg("--go-cacheprog")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut go = Go {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 1,
        };
        let hello = go.read()?;
        assert_eq!(
            hello["KnownCommands"],
            serde_json::json!(["get", "put", "close"])
        );
        Ok(go)
    }

    fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        self.stdout.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }

    fn request(&mut self, mut request: Value, body: Option<&[u8]>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        request["ID"] = id.into();
        writeln!(self.stdin, "{request}")?;
        if let Some(body) = body {
            writeln!(self.stdin, "\"{}\"", BASE64.encode(body))?;
        }
        let response = self.read()?;
        assert_eq!(response["ID"], id);
        Ok(response)
    }

    fn put(&mut self, action: u8, output: u8, body: &[u8]) -> Result<Value> {
        self.request(
            serde_json::json!({
                "Command": "put",
                "ActionID": BASE64.encode([action; 32]),
                "OutputID": BASE64.encode([output; 32]),
                "BodySize": body.len(),
            }),
            Some(body),
        )
    }

    fn get(&mut self, action: u8) -> Result<Value> {
        self.request(
            serde_json::json!({"Command": "get", "ActionID": BASE64.encode([action; 32])}),
            None,
        )
    }

    fn close(mut self) -> Result<()> {
        self.request(serde_json::json!({"Command": "close"}), None)?;
        assert!(self.child.wait()?.success());
        Ok(())
    }
}

fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[test]
#[serial]
fn test_go_cacheprog_shares_entries_between_runs() -> Result<()> {
    let server = Server::start(None)?;

    let mut go = Go::spawn(&server)?;
    let stored = go.put(1, 101, b"object file")?;
    let staged = Path::new(stored["DiskPath"].as_str().unwrap()).to_owned();
    assert_eq!(fs::read(&staged)?, b"object file");
    go.close()?;
    assert!(
        !staged.exists(),
        "staging directory outlived the cache program"
    );

    let mut go = Go::spawn(&server)?;
    let hit = go.get(1)?;
    assert_eq!(hit["OutputID"], BASE64.encode([101; 32]));
    assert_eq!(hit["Size"], 11);
    assert_eq!(fs::read(hit["DiskPath"].as_str().unwrap())?, b"object file");
    assert_eq!(go.get(2)?["Miss"], true);
    go.close()?;

    let stats = server.stats()?;
    assert_eq!(stats["cache_hits"]["counts"]["Go"], 1);
    assert_eq!(stats["cache_misses"]["counts"]["Go"], 1);
    assert_eq!(stats["cache_writes"], 1);
    Ok(())
}

// An entry the server refuses must cost that entry only, not the connection
// it was sent on.
#[test]
#[serial]
fn test_go_cacheprog_survives_oversized_entry() -> Result<()> {
    let server = Server::start(Some(FRAME_LIMIT))?;

    let mut go = Go::spawn(&server)?;
    let big = incompressible(4 * FRAME_LIMIT);
    let stored = go.put(1, 101, &big)?;
    assert_eq!(fs::read(stored["DiskPath"].as_str().unwrap())?, big);
    for action in 100..200 {
        assert_eq!(go.get(action)?["Miss"], true);
    }
    go.put(2, 102, b"small")?;
    go.close()?;

    let mut go = Go::spawn(&server)?;
    assert_eq!(go.get(1)?["Miss"], true);
    assert_eq!(go.get(2)?["Size"], 5);
    go.close()?;

    let stats = server.stats()?;
    assert_eq!(stats["cache_write_errors"], 1);
    assert_eq!(stats["cache_read_errors"], 0);
    assert_eq!(stats["cache_writes"], 1);
    Ok(())
}

fn go_supports_cacheprog() -> bool {
    let Ok(output) = Command::new("go").args(["env", "GOVERSION"]).output() else {
        return false;
    };
    let version = String::from_utf8_lossy(&output.stdout);
    version
        .trim()
        .strip_prefix("go1.")
        .and_then(|v| v.split('.').next())
        .and_then(|minor| minor.parse::<u32>().ok())
        .is_some_and(|minor| minor >= 24)
}

#[test]
#[serial]
fn test_go_build_uses_cache() -> Result<()> {
    if !go_supports_cacheprog() {
        eprintln!("skipping: needs go 1.24 or newer");
        return Ok(());
    }
    let server = Server::start(None)?;
    let project = server.tempdir.path().join("project");
    fs::create_dir(&project)?;
    fs::write(
        project.join("go.mod"),
        "module example.com/hello\n\ngo 1.24\n",
    )?;
    fs::write(
        project.join("main.go"),
        "package main\n\nfunc main() { println(\"hello\") }\n",
    )?;

    let build = |out: &str| -> Result<()> {
        let mut cacheprog = SCCACHE_BIN.as_os_str().to_owned();
        cacheprog.push(" --go-cacheprog");
        Command::new("go")
            .args(["build", "-o", out, "."])
            .current_dir(&project)
            .env("GOCACHEPROG", cacheprog)
            .env("GOCACHE", server.tempdir.path().join("gocache"))
            .env("GOFLAGS", "")
            .env("GOTOOLCHAIN", "local")
            .env("SCCACHE_DIR", server.tempdir.path().join("cache"))
            .env_remove("SCCACHE_MAX_FRAME_LENGTH")
            .assert()
            .try_success()?;
        Ok(())
    };

    build("first")?;
    let stats = server.stats()?;
    assert!(stats["cache_writes"].as_u64().unwrap() > 0);
    assert!(stats["cache_hits"]["counts"]["Go"].is_null());

    build("second")?;
    let stats = server.stats()?;
    assert!(stats["cache_hits"]["counts"]["Go"].as_u64().unwrap() > 0);
    assert_eq!(stats["cache_write_errors"], 0);
    assert_eq!(
        fs::read(project.join("first"))?,
        fs::read(project.join("second"))?
    );
    Command::new(project.join("second"))
        .assert()
        .try_success()?
        .try_stderr("hello\n")?;
    Ok(())
}
