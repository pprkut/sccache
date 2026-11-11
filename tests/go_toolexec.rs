//! Tests for `go build -toolexec sccache`.
//!
//! Any copyright is dedicated to the Public Domain.
//! http://creativecommons.org/publicdomain/zero/1.0/
pub mod helpers;

use anyhow::Result;
use assert_cmd::prelude::*;
use fs_err as fs;
use helpers::{SCCACHE_BIN, SccacheTest};
use serde_json::Value;
use serial_test::serial;
use std::path::Path;
use std::process::Command;

fn go_is_installed() -> bool {
    Command::new("go")
        .args(["env", "GOVERSION"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn write_project(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir.join("greeting"))?;
    fs::write(dir.join("go.mod"), "module example.com/hello\n\ngo 1.21\n")?;
    fs::write(
        dir.join("main.go"),
        "package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/hello/greeting\"\n)\n\n\
         func main() { fmt.Println(greeting.Text) }\n",
    )?;
    fs::write(
        dir.join("greeting/greeting.go"),
        "package greeting\n\nimport _ \"embed\"\n\n//go:embed text\nvar Text string\n",
    )?;
    fs::write(dir.join("greeting/text"), "hello")?;
    Ok(())
}

fn go_build(test: &SccacheTest, project: &Path, name: &str, toolexec: bool) -> Result<Vec<u8>> {
    let root = test.tempdir.path();
    let binary = root.join(name);
    let mut cmd = Command::new("go");
    cmd.args(["build", "-trimpath", "-o"]).arg(&binary);
    if toolexec {
        cmd.arg("-toolexec").arg(SCCACHE_BIN.as_os_str());
    }
    cmd.current_dir(project)
        .envs(test.env.iter().cloned())
        // Nothing may come from the go command's own cache.
        .env("GOCACHE", root.join(format!("gocache-{name}")))
        .env("GOFLAGS", "")
        .env("GOTOOLCHAIN", "local")
        .env_remove("GOCACHEPROG")
        .assert()
        .try_success()?;
    Ok(fs::read(binary)?)
}

fn go_compiles(test: &SccacheTest, what: &str) -> Result<u64> {
    let stats: Value = serde_json::from_slice(&test.show_stats()?.get_output().stdout)?;
    Ok(stats["stats"][what]["counts"]["Go compile"]
        .as_u64()
        .unwrap_or(0))
}

#[test]
#[serial]
fn test_go_toolexec_caches_compiles() -> Result<()> {
    if !go_is_installed() {
        eprintln!("skipping: needs go");
        return Ok(());
    }
    let test = SccacheTest::new(None)?;
    let project = test.tempdir.path().join("project");
    write_project(&project)?;

    let plain = go_build(&test, &project, "plain", false)?;
    assert_eq!(go_compiles(&test, "cache_misses")?, 0);

    assert_eq!(go_build(&test, &project, "first", true)?, plain);
    let compiles = go_compiles(&test, "cache_misses")?;
    assert!(
        compiles > 2,
        "only {compiles} compiles went through sccache"
    );
    assert_eq!(go_compiles(&test, "cache_hits")?, 0);

    // The same sources somewhere else: -trimpath makes the directory irrelevant.
    let moved = test.tempdir.path().join("moved");
    write_project(&moved)?;
    assert_eq!(go_build(&test, &moved, "second", true)?, plain);
    // Packages that cgo generates code for name the work directory.
    let hits = go_compiles(&test, "cache_hits")?;
    let misses = go_compiles(&test, "cache_misses")? - compiles;
    assert!(
        misses < 10 && hits + misses == compiles,
        "{hits} hits, {misses} misses"
    );

    fs::write(moved.join("greeting/text"), "goodbye")?;
    // The package that embeds the file, and the one importing it.
    assert_ne!(go_build(&test, &moved, "third", true)?, plain);
    assert_eq!(go_compiles(&test, "cache_hits")?, 2 * hits - 2);
    assert_eq!(
        go_compiles(&test, "cache_misses")?,
        compiles + 2 * misses + 2
    );

    let output = Command::new(test.tempdir.path().join("third")).output()?;
    assert_eq!(output.stdout, b"goodbye\n");
    Ok(())
}
