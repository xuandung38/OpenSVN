//! Checks a manifest endpoint with the real `tauri-plugin-updater` and, optionally, downloads the
//! artifact (the plugin verifies its minisign signature while downloading).
//!
//! ```text
//! updater-harness --endpoint URL --pubkey TEXT --current 0.1.0 [--header K=V]... [--download]
//!                 [--download-accept VALUE|keep] [--allowed-download-host HOST [--insecure-test]]
//! ```
//! `--header` applies to the manifest request. Before downloading, the harness replaces `Accept`
//! with `application/octet-stream` (the GitHub asset API needs it; the plugin would otherwise reuse
//! the manifest `Accept`), unless `--download-accept keep` reproduces the pitfall.
//! `--allowed-download-host` is the reference for the app-side check that must run before the
//! bearer token is sent: https scheme (waived by `--insecure-test`) and an exact host match.
//! Prints one JSON line: `{"available":false}` or
//! `{"available":true,"version":"0.2.0","downloaded_bytes":N}`. Exits 1 on any failure
//! (unreachable endpoint, bad manifest, signature mismatch).

use std::process::ExitCode;

use tauri_plugin_updater::UpdaterExt;

struct Args {
    endpoint: String,
    pubkey: String,
    current: semver::Version,
    headers: Vec<(String, String)>,
    download: bool,
    download_accept: Option<String>,
    allowed_download_host: Option<String>,
    insecure_test: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut endpoint = None;
    let mut pubkey = None;
    let mut current = None;
    let mut headers = Vec::new();
    let mut download = false;
    let mut download_accept = Some("application/octet-stream".to_owned());
    let mut allowed_download_host = None;
    let mut insecure_test = false;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--endpoint" => endpoint = Some(value("--endpoint")?),
            "--pubkey" => pubkey = Some(value("--pubkey")?),
            "--current" => {
                let text = value("--current")?;
                current = Some(
                    semver::Version::parse(&text).map_err(|e| format!("--current {text}: {e}"))?,
                );
            }
            "--header" => {
                let kv = value("--header")?;
                let (k, v) = kv.split_once('=').ok_or("--header expects K=V")?;
                headers.push((k.to_owned(), v.to_owned()));
            }
            "--download" => download = true,
            "--download-accept" => {
                let v = value("--download-accept")?;
                download_accept = (v != "keep").then_some(v);
            }
            "--allowed-download-host" => {
                allowed_download_host = Some(value("--allowed-download-host")?);
            }
            "--insecure-test" => insecure_test = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        endpoint: endpoint.ok_or("--endpoint is required")?,
        pubkey: pubkey.ok_or("--pubkey is required")?,
        current: current.ok_or("--current is required")?,
        headers,
        download,
        download_accept,
        allowed_download_host,
        insecure_test,
    })
}

async fn run(args: Args) -> Result<String, Box<dyn std::error::Error>> {
    // The plugin reads `plugins.updater` from the app config, exactly like the shipped app will.
    let mut context = tauri::test::mock_context(tauri::test::noop_assets());
    context.config_mut().plugins.0.insert(
        "updater".into(),
        serde_json::json!({ "pubkey": args.pubkey, "endpoints": [args.endpoint] }),
    );
    let app = tauri::test::mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(context)?;
    let current = args.current;
    let mut builder = app
        .updater_builder()
        .version_comparator(move |_installed, remote| remote.version > current);
    for (k, v) in &args.headers {
        builder = builder.header(k.as_str(), v.as_str())?;
    }
    let updater = builder.build()?;
    let Some(mut update) = updater.check().await? else {
        return Ok(serde_json::json!({ "available": false }).to_string());
    };
    if let Some(host) = &args.allowed_download_host {
        let url = &update.download_url;
        if url.scheme() != "https" && !args.insecure_test {
            return Err(format!("download url is not https: {url}").into());
        }
        if url.host_str() != Some(host.as_str()) {
            return Err(format!("download host {:?} is not {host}", url.host_str()).into());
        }
    }
    if let Some(accept) = &args.download_accept {
        update
            .headers
            .insert("accept", tauri::http::HeaderValue::from_str(accept)?);
    }
    let downloaded = if args.download {
        update.download(|_, _| {}, || {}).await?.len()
    } else {
        0
    };
    Ok(serde_json::json!({
        "available": true,
        "version": update.version,
        "downloaded_bytes": downloaded,
    })
    .to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
