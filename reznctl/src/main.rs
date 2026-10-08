use anyhow::{Context, Result};
use serde_json::Value;
use std::{env, fs};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        eprintln!("Usage: reznctl <apply-url> <name> <signed-ir.json>");
        std::process::exit(1);
    }

    let url = &args[1];
    let name = &args[2];
    let json_path = &args[3];
    let raw = fs::read_to_string(json_path).context("reading IR file")?;

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
