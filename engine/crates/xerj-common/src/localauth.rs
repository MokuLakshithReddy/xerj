//! Local admin-key discovery and the loopback guard that makes it safe.
//!
//! The shipped default config has `[auth] enabled = true`, and the server
//! mints `<data_dir>/admin.key` on first start. Commands that talk to a
//! node from the same machine (`xerj mcp`, `xerj autoindex`, `xerj init`'s
//! advice) can read that file instead of demanding the user paste a key —
//! but only when the target is provably loopback, because a locally
//! readable key sent anywhere else is a credential leak, not a
//! convenience.
//!
//! Both helpers started life in `xerj-autoindex/src/cli.rs` (the autoindex
//! 401 fix); `xerj mcp` hit the identical failure mode (#961), so they
//! moved here rather than being duplicated. The invariants live with the
//! tests in `xerj-autoindex` (`url_is_loopback_is_true_only_for_this_machine`,
//! `userinfo_cannot_disguise_a_remote_host_as_loopback`, and the
//! `discover_local_admin_key` sandbox tests) — the calling crates keep
//! wiring tests of their own.
//!
//! The URL parsing uses a real parser (`url::Url`, the same type reqwest
//! re-exports) rather than string surgery, because the hand-rolled version
//! of this was wrong in a way that leaked credentials: splitting on the
//! last `:` treats the userinfo in `http://localhost:9200@evil.com/` as a
//! host:port pair, judges it loopback, and sends the admin key to
//! `evil.com`. `Url::host_str` resolves that to `evil.com`, which is the
//! whole point of using it.

use std::path::PathBuf;

/// True when `url`'s host is this machine, so a locally readable admin key
/// is the right credential to send. Anything else — a LAN address, a
/// hostname, a remote deployment — must be given a key explicitly.
pub fn url_is_loopback(url: &str) -> bool {
    // A schemeless `localhost:9200` parses as scheme `localhost`, path `9200`,
    // with no host at all, so retry those through `http://`. The retry still
    // goes through the parser: `localhost:9200@evil.com` becomes
    // `http://localhost:9200@evil.com`, whose host is `evil.com`, not loopback.
    let parsed = match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u,
        _ => match url::Url::parse(&format!("http://{url}")) {
            Ok(u) => u,
            // Unparseable is not loopback. Failing closed here costs a user
            // with an exotic URL one explicit --api-key; failing open costs
            // them the key itself.
            Err(_) => return false,
        },
    };
    match parsed.host_str() {
        // `host_str` strips the brackets from `[::1]` and does not lowercase
        // an IPv6 literal, so compare case-insensitively and cover both forms.
        Some(h) => {
            let h = h.trim_start_matches('[').trim_end_matches(']');
            h.eq_ignore_ascii_case("localhost") || h == "127.0.0.1" || h == "::1" || h == "0.0.0.0"
        }
        None => false,
    }
}

/// The admin key a local server wrote for itself, if we can find it.
///
/// Checked in the order a user is most likely to have created them: the
/// working directory's data dir (what the quickstart tells you to use), then
/// the documented package install location. Absent or unreadable is not an
/// error — the caller falls back to the actionable 401 message.
///
/// Returns the bare key (trimmed) and the file it came from, so the caller
/// can announce the path — never silently — and format the header per its
/// own wire convention (`ApiKey <key>`). Relative candidates are resolved
/// against the current directory before being returned: the announcement
/// must name a path the reader can act on even when the calling process was
/// spawned with an arbitrary cwd, which is exactly what MCP hosts do.
pub fn discover_local_admin_key() -> Option<(String, PathBuf)> {
    // Relative candidates resolve against the current directory (the "./"
    // prefix is dropped so the announced path reads cleanly); the first two
    // are the quickstart's own `--data-dir ./data` and its sibling spelling.
    const CANDIDATES: &[&str] = &[
        "data/admin.key",
        "xerj-data/admin.key",
        "/var/lib/xerj/admin.key",
    ];
    let cwd = std::env::current_dir().ok();
    let mut paths: Vec<PathBuf> = CANDIDATES
        .iter()
        .map(|c| {
            let p = PathBuf::from(c);
            match (&cwd, p.is_absolute()) {
                (Some(cwd), false) => cwd.join(p),
                _ => p,
            }
        })
        .collect();
    if let Some(home) = crate::home::home_dir() {
        paths.push(home.join(".xerj/brain/admin.key"));
        paths.push(home.join(".xerj/admin.key"));
    }
    paths.into_iter().find_map(|p| {
        std::fs::read_to_string(&p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|k| (k, p))
    })
}
