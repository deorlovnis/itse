//! `itse-fixture tool <name> [args…]`: delegate stand-ins. Every call is appended as one JSON line
//! (`{tool, argv, cwd}`) to `ITSE_FIXTURE_LOG` when set, so a test can check an exact argv.
//!
//! - `curl`: argv-compatible with `curl --fail-with-body -sSL --proto =https -w FMT [-C -] -o FILE
//!   URL`. It serves `https://<host>/<path>` from `$ITSE_FIXTURE_FILES/<host>/<path>`, resumes like
//!   curl (a part as long as the file is a 416), prints `<http_connect> <http_code> <url>` as `-w`
//!   would through the door, and plays the door's refusals: `blocked.test` (CONNECT 403),
//!   `badtls.test` (CONNECT 502), `nowrite.test` (in-tunnel 403), `mismatch.test` (421),
//!   `encoded.test` (502), the in-tunnel ones with the door's `halti: ` body appended to FILE;
//!   `ratelimit.test`, the site's own in-tunnel 403; and `norange.test`, which ignores Range (exit 33).
//! - `keytool`: writes the `-keystore` file with the `-file` CA's bytes.
//! - anything else prints the JSON line on stdout; `slow` sleeps 30 s first.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use std::{env, thread};

use serde_json::json;

pub fn run(args: &[String], mode: &str) -> ExitCode {
    let Some((name, argv)) = args.split_first() else {
        eprintln!("itse-fixture tool: which tool?");
        return ExitCode::from(2);
    };
    if mode == "slow" {
        thread::sleep(Duration::from_secs(30));
    }
    let line = json!({"tool": name, "argv": argv, "cwd": env::current_dir().unwrap_or_default()}).to_string();
    if let Some(log) = env::var_os("ITSE_FIXTURE_LOG") {
        let _ = OpenOptions::new().create(true).append(true).open(log).and_then(|mut f| writeln!(f, "{line}"));
    }
    match name.as_str() {
        "curl" => curl(argv),
        "keytool" => keytool(argv),
        _ => {
            println!("{line}");
            ExitCode::SUCCESS
        }
    }
}

fn after<'a>(argv: &'a [String], flag: &str) -> Option<&'a String> {
    argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1))
}

fn curl(argv: &[String]) -> ExitCode {
    let (url, out, resume) = (argv.last().cloned().unwrap_or_default(), after(argv, "-o").cloned().unwrap_or_default(), after(argv, "-C").is_some());
    let rest = url.strip_prefix("https://").unwrap_or("");
    let host = rest.split('/').next().unwrap_or("");
    let fail = |code: u8, w: &str, why: &str| {
        print!("{w} {url}");
        eprintln!("curl: ({code}) {why}");
        ExitCode::from(code)
    };
    match host {
        _ if !url.starts_with("https://") => return fail(1, "000 000", "Protocol \"http\" disabled"),
        "blocked.test" => return fail(56, "403 000", "CONNECT tunnel failed, response 403"),
        "badtls.test" => return fail(56, "502 000", "CONNECT tunnel failed, response 502"),
        "nowrite.test" | "mismatch.test" | "encoded.test" | "ratelimit.test" => {
            let code = match host {
                "mismatch.test" => "421",
                "encoded.test" => "502",
                _ => "403",
            };
            let body = if host == "ratelimit.test" { "{\"message\": \"API rate limit exceeded\"}" } else { "halti: refused here\n" };
            let _ = OpenOptions::new().create(true).append(true).open(&out).and_then(|mut f| f.write_all(body.as_bytes()));
            return fail(22, &format!("200 {code}"), &format!("The requested URL returned error: {code}"));
        }
        "norange.test" if resume => return fail(33, "200 200", "HTTP server doesn't seem to support byte ranges. Cannot resume."),
        _ => {}
    }
    let file = PathBuf::from(env::var_os("ITSE_FIXTURE_FILES").unwrap_or_default()).join(rest);
    let Ok(bytes) = fs::read(&file) else { return fail(22, "200 404", "The requested URL returned error: 404") };
    let have = if resume { fs::metadata(&out).map_or(0, |m| m.len() as usize) } else { 0 };
    if have >= bytes.len() && have > 0 {
        return fail(22, "200 416", "The requested URL returned error: 416");
    }
    let written = OpenOptions::new().create(true).write(true).append(resume).truncate(!resume).open(&out).and_then(|mut f| f.write_all(&bytes[have..]));
    if let Err(e) = written {
        return fail(23, "200 200", &format!("Failure writing output to destination: {e}"));
    }
    print!("200 200 {url}");
    ExitCode::SUCCESS
}

fn keytool(argv: &[String]) -> ExitCode {
    let (Some(ca), Some(store)) = (after(argv, "-file"), after(argv, "-keystore")) else {
        eprintln!("keytool error: -file and -keystore are needed");
        return ExitCode::from(1);
    };
    match fs::read(ca).and_then(|pem| fs::write(store, [b"PKCS12 of ".as_slice(), &pem].concat())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("keytool error: {e}");
            ExitCode::from(1)
        }
    }
}
