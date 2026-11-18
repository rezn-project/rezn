use anyhow::{Context, Result};
use common::ir_v2;
use serde_json::Value;
use std::{env, fs, process::ExitCode};

const USAGE: &str = "Usage:
  reznctl <apply-url> <name> <signed-ir.json>   apply a signed program
  reznctl validate <program.json>                validate an IR v2 program or signed envelope offline";

fn main() -> Result<ExitCode> {
    let args: Vec<String> = env::args().collect();
    match args.as_slice() {
        [_, command, path] if command == "validate" => validate(path),
        [_, url, name, path] => apply(url, name, path).map(|()| ExitCode::SUCCESS),
        _ => {
            eprintln!("{USAGE}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Validates an IR v2 program, given bare or inside a signed envelope (`{program, signature}`). The signature is
/// not checked here; `/apply` verifies it.
fn validate(path: &str) -> Result<ExitCode> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let document: Value = serde_json::from_str(&raw).context("parsing JSON")?;
    let program = match document.get("program") {
        Some(program) if document.get("ir").is_none() => program,
        _ => &document,
    };
    if program.is_array() {
        eprintln!("{path}: an IR v1 instruction array; IR v1 is retired and Rezn accepts only IR v2 programs");
        return Ok(ExitCode::FAILURE);
    }
    match ir_v2::validate(program) {
        Ok(program) => {
            println!(
                "{path}: valid IR v2 program for deployment {:?} ({} resources)",
                program.deployment,
                program.resources.len()
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(issues) => {
            for issue in &issues {
                println!("{issue}");
            }
            eprintln!("{path}: {} issue(s)", issues.len());
            Ok(ExitCode::FAILURE)
        }
    }
}

fn apply(url: &str, name: &str, path: &str) -> Result<()> {
    let raw = fs::read_to_string(path).context("reading IR file")?;

    // Make the HTTP request to the Rezn Runtime
    let client = reqwest::blocking::Client::new();
    let payload = serde_json::json!({
        "name": name,
        "instruction_wrapper": serde_json::from_str::<Value>(&raw).context("parsing JSON")?,
    });

    let response = client
        .post(url)
        .json(&payload)
        .send()
        .context("sending HTTP request")?;

    let status = response.status();
    let body = response.text().context("reading HTTP response")?;
    anyhow::ensure!(status.is_success(), "Apply failed: HTTP {status}: {body}");
    println!("{body}");

    Ok(())
}
