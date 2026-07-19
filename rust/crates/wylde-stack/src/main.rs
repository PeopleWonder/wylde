//! `wylde-stack` — the resolver the launcher shells out to.
//!
//! `launch_wylde.ps1` used to reimplement binary resolution in PowerShell,
//! which is precisely how it drifted from the updater. It now asks this
//! binary instead, so the shortcut path and the update path are the *same*
//! code by construction rather than two implementations kept in sync by
//! remembering to.
//!
//! ```text
//! wylde-stack resolve [--json]   # where every stack binary resolves to
//! wylde-stack roster [--json]    # what the stack consists of (no disk check)
//! wylde-stack current            # print the current-pointer directory
//! ```
//!
//! `resolve` exits non-zero when the daemon cannot be resolved — that is the
//! one condition under which launching is meaningless.

use std::process::ExitCode;

use wylde_stack::{current, roster, service_name as sn};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let cmd = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
        .unwrap_or("resolve");

    match cmd {
        "resolve" => resolve(json),
        "roster" => {
            let entries = roster::roster();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries).unwrap());
            } else {
                for b in entries {
                    println!("{}\t{:?}\t{}", b.name, b.tier, b.image);
                }
            }
            ExitCode::SUCCESS
        }
        "current" => match current::current_dir() {
            Some(dir) => {
                println!("{}", dir.display());
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("no `current` pointer set (build-tree fallback in effect)");
                ExitCode::from(2)
            }
        },
        other => {
            eprintln!("unknown command `{other}`; expected resolve|roster|current");
            ExitCode::from(64)
        }
    }
}

fn resolve(json: bool) -> ExitCode {
    let stack = current::resolve();

    if json {
        println!("{}", serde_json::to_string_pretty(&stack).unwrap());
    } else {
        println!("source: {:?}", stack.source);
        for b in &stack.binaries {
            match &b.path {
                Some(p) => println!("{}\t{}", b.name, p.display()),
                None => println!("{}\t<missing>", b.name),
            }
        }
    }

    // The daemon is the one hard requirement. A missing GUI or an unbuilt
    // service is reported (in the payload / on stderr) but is not fatal —
    // those are normal states on a partially-built tree.
    match stack.daemon() {
        Ok(_) => {
            let missing = stack.missing();
            if !missing.is_empty() {
                eprintln!("warning: not built: {}", missing.join(", "));
            }
            if stack.path_of(sn::GUI).is_none() {
                eprintln!("warning: {} is missing", sn::GUI);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
