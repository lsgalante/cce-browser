//! The sync worker — phase 3: when a pass runs, and what it says.
//!
//! One thread, `cce-raindrop`, for the life of the browser once the setting
//! has been on. It polls the in-memory bookmarks every [`POLL`] instead of
//! being told about edits: a bookmark changes from the star, Ctrl+D, the
//! bookmarks menu and the `cce://bookmarks` page, and comparing the rows
//! (no I/O, a lock held for a copy) catches all of them without a hook in
//! each. A change syncs once the rows have held still for one poll, so a
//! burst of edits is one pass; Raindrop's own changes arrive with the pass
//! every [`FULL`]; the page's "sync now" asks for one at once.
//!
//! A pass is [`run_pass`]: fetch, plan, apply Raindrop's half, apply the local
//! half through `Bookmarks::edit_rows` (one locked edit — no star can land in
//! the middle of it), and save the base last. Its outcome becomes the status
//! line on `cce://bookmarks`, and what it changed is appended to the sync log
//! (`raindrop-sync.log`, beside the base) — one line per bookmark, so "which
//! ones?" has an answer after the status line and the process are gone.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{api, apply_local, load_base, plan, save_base, settle_base, Local};
use crate::pages::{Bookmarks, SyncNote, SYNC_FORCE};

/// How often the rows are compared, and the stillness a change waits for.
const POLL: Duration = Duration::from_secs(2);
/// A full pass this often while nothing local changes: Raindrop's side.
const FULL: Duration = Duration::from_secs(600);

/// The sync log beside a base: `raindrop-sync.tsv` → `raindrop-sync.log`.
pub fn log_path(base_path: &Path) -> std::path::PathBuf {
    base_path.with_extension("log")
}

/// Past this size the log keeps only its newer half — months of passes, kept
/// to a size nobody has to think about.
const LOG_MAX: usize = 512 * 1024;

/// Append `events` (`(what, link, title)`) to the log, one line each:
/// `time \t what \t link \t title`. Best effort — a log that cannot be
/// written must not fail a pass that already happened — but said once.
pub fn append_log(path: &Path, events: &[(String, String, String)]) {
    if events.is_empty() {
        return;
    }
    let now = api::iso8601(super::unix_now());
    let flat = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    for (what, link, title) in events {
        text.push_str(&format!("{now}\t{}\t{}\t{}\n", flat(what), flat(link), flat(title)));
    }
    if text.len() > LOG_MAX {
        let cut = text.len() - LOG_MAX / 2;
        let from = text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
        text = text[from..].to_string();
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("log.tmp");
    if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path)) {
        log::warn!("raindrop: could not write {}: {e}", path.display());
    }
}

/// What one pass did.
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub added_here: usize,
    pub removed_here: usize,
    pub changed_here: usize,
    pub added_there: usize,
    pub trashed_there: usize,
    pub renamed_there: usize,
    /// Local operations left for the next pass (edited mid-pass).
    pub deferred: usize,
    /// Raindrop calls that failed; each is retried next pass.
    pub errors: Vec<String>,
}

impl Summary {
    /// The status line: what moved, or "in sync".
    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        let mut say = |n: usize, what: &str| {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        };
        say(self.added_here, "added here");
        say(self.removed_here, "removed here");
        say(self.changed_here, "updated here");
        say(self.added_there, "added to Raindrop");
        say(self.trashed_there, "moved to Raindrop's trash");
        say(self.renamed_there, "renamed in Raindrop");
        let mut s = if parts.is_empty() { "in sync".to_string() } else { format!("synced — {}", parts.join(", ")) };
        if !self.errors.is_empty() {
            s.push_str(&format!(" ({} failed, retrying next pass)", self.errors.len()));
        }
        s
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Synced(Summary),
    /// The guard refused the pass; nothing was changed on either side.
    Refused(String),
}

fn locals(rows: Vec<(u64, String, String)>) -> Vec<Local> {
    rows.into_iter().map(|(ts, url, title)| Local { url, title, ts }).collect()
}

/// One pass against `client`'s Unsorted. `force` runs a plan the guard
/// refused — only ever from the page's "sync anyway", after a person has read
/// why it was refused.
pub fn run_pass(
    client: &api::Client,
    bookmarks: &Bookmarks,
    base_path: &Path,
    force: bool,
) -> Result<Outcome, String> {
    let remote = client.fetch(api::UNSORTED).map_err(|e| e.to_string())?;
    let snapshot = locals(bookmarks.rows());
    let base = load_base(base_path);
    let log = log_path(base_path);
    let plan = match plan(&snapshot, &remote, &base) {
        Ok(p) => p,
        Err(refused) if force => {
            log::warn!("raindrop: running a refused pass on request ({})", refused.reason);
            append_log(&log, &[("forced".into(), String::new(), refused.reason.clone())]);
            refused.plan
        }
        Err(refused) => {
            append_log(&log, &[("refused".into(), String::new(), refused.reason.clone())]);
            return Ok(Outcome::Refused(refused.reason));
        }
    };
    let applied = api::apply_remote(client, api::UNSORTED, &plan).map_err(|e| e.to_string())?;
    let touches_local = !(plan.relink_local.is_empty()
        && plan.rename_local.is_empty()
        && plan.delete_local.is_empty()
        && plan.add_local.is_empty());
    // Only rewrite the file when there is something to write: a pass every
    // ten minutes that changes nothing must not touch the disk.
    // The log says what *happened*: the local lines are the difference the
    // edit actually made (a deferred operation made none), the Raindrop lines
    // the calls that succeeded.
    let mut events: Vec<(String, String, String)> = Vec::new();
    let skipped = if touches_local {
        bookmarks.edit_rows(|rows| {
            let before: Vec<(u64, String, String)> = rows.clone();
            let mut current = locals(std::mem::take(rows));
            let skipped = apply_local(&mut current, &snapshot, &plan);
            *rows = current.into_iter().map(|l| (l.ts, l.url, l.title)).collect();
            for (_, url, title) in rows.iter() {
                match before.iter().find(|b| b.1 == *url) {
                    None => events.push(("added here".into(), url.clone(), title.clone())),
                    Some(b) if b.2 != *title => {
                        events.push(("renamed here".into(), url.clone(), format!("{} → {title}", b.2)))
                    }
                    Some(_) => {}
                }
            }
            for (_, url, title) in &before {
                if !rows.iter().any(|r| r.1 == *url) {
                    events.push(("removed here".into(), url.clone(), title.clone()));
                }
            }
            skipped
        })
    } else {
        super::Skipped::default()
    };
    let link_of = |id: &super::RaindropId| {
        remote.iter().find(|r| r.id == *id).map(|r| (r.link.clone(), r.title.clone())).unwrap_or_default()
    };
    for (url, _) in &applied.created {
        let title = plan.create_remote.iter().find(|l| l.url == *url).map(|l| l.title.clone()).unwrap_or_default();
        events.push(("added to Raindrop".into(), url.clone(), title));
    }
    for id in plan.trash_remote.iter().filter(|id| !applied.failed_trash.contains(id)) {
        let (link, title) = link_of(id);
        events.push(("moved to Raindrop's trash".into(), link, title));
    }
    for (id, new) in plan.rename_remote.iter().filter(|(id, _)| !applied.failed_renames.contains(id)) {
        let (link, old) = link_of(id);
        events.push(("renamed in Raindrop".into(), link, format!("{old} → {new}")));
    }
    for e in &applied.errors {
        events.push(("failed".into(), String::new(), e.clone()));
    }
    append_log(&log, &events);
    // Last: a pass that dies before this point leaves the old base, and the
    // next pass redoes the work rather than misreading it.
    let new_base = settle_base(&plan, &base, &applied, &skipped);
    if new_base != base {
        save_base(base_path, &new_base).map_err(|e| format!("could not save the sync state: {e}"))?;
    }
    Ok(Outcome::Synced(Summary {
        added_here: plan.add_local.len(),
        removed_here: plan.delete_local.len(),
        changed_here: plan.rename_local.len() + plan.relink_local.len(),
        added_there: applied.created.len(),
        trashed_there: plan.trash_remote.len() - applied.failed_trash.len(),
        renamed_there: plan.rename_remote.len() - applied.failed_renames.len(),
        deferred: skipped.count,
        errors: applied.errors,
    }))
}

/// A code for the "sync anyway" link. Not cryptographic, and it needs not be:
/// it only has to be something a web page cannot know, and the page that
/// shows it is the only place it is written.
fn force_code() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(super::unix_now());
    h.finish()
}

/// One pass, start to status line: the token, the client, `run_pass`.
fn pass(bookmarks: &Bookmarks, force: bool) {
    let note = |text: String, force_code: Option<u64>| {
        bookmarks.set_sync_note(Some(SyncNote { text, at: super::unix_now(), force_code }));
    };
    let token = match crate::accounts::raindrop_token() {
        Ok(t) => t,
        Err(e) => {
            log::warn!("raindrop: {e}");
            note(e, None);
            return;
        }
    };
    let client = api::Client::new(token);
    match run_pass(&client, bookmarks, &super::state_path(), force) {
        Ok(Outcome::Synced(s)) => {
            for e in &s.errors {
                log::warn!("raindrop: {e}");
            }
            log::info!("raindrop: {}", s.text());
            note(s.text(), None);
        }
        Ok(Outcome::Refused(reason)) => {
            log::warn!("raindrop: pass refused: {reason}");
            // The same code for as long as passes keep being refused: a page
            // already showing "sync anyway" must stay able to use it when a
            // later pass — the periodic one, a reload — refuses again.
            let code = bookmarks.sync_force_code().unwrap_or_else(force_code);
            note(format!("not synced: {reason}"), Some(code));
        }
        Err(e) => {
            log::warn!("raindrop: {e}");
            append_log(&log_path(&super::state_path()), &[("not synced".into(), String::new(), e.clone())]);
            note(format!("not synced: {e}"), None);
        }
    }
}

/// Start the worker. `enabled` is the setting, live: off, the worker idles
/// and the page shows no status line; on again, it syncs at once.
pub fn spawn(bookmarks: Arc<Bookmarks>, enabled: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("cce-raindrop".to_string())
        .spawn(move || {
            let mut last_rows = None;
            let mut changed = false;
            let mut next_full = Instant::now();
            let mut was_on = false;
            loop {
                std::thread::sleep(POLL);
                let on = enabled.load(Ordering::SeqCst);
                if !on {
                    if was_on {
                        bookmarks.set_sync_note(None);
                    }
                    was_on = false;
                    continue;
                }
                if !was_on {
                    // Just turned on (or launched on): sync now.
                    was_on = true;
                    next_full = Instant::now();
                }
                let request = bookmarks.take_sync_request();
                let rows = bookmarks.rows();
                if last_rows.as_ref() != Some(&rows) {
                    // Changed since the last look: wait one poll for it to
                    // hold still, so a burst of edits is one pass.
                    changed = last_rows.is_some();
                    last_rows = Some(rows);
                    if request == 0 {
                        continue;
                    }
                }
                if request != 0 || changed || Instant::now() >= next_full {
                    pass(&bookmarks, request == SYNC_FORCE);
                    changed = false;
                    next_full = Instant::now() + FULL;
                    // The pass's own edits are not a local change to sync.
                    last_rows = Some(bookmarks.rows());
                }
            }
        })
        .expect("spawn the Raindrop sync worker");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Secret;

    #[test]
    fn the_log_keeps_its_newer_half() {
        let dir = std::env::temp_dir().join(format!("cce-raindrop-log-{}", std::process::id()));
        let path = dir.join("raindrop-sync.log");
        let long = "x".repeat(1000);
        for i in 0..700 {
            append_log(&path, &[("added here".into(), format!("https://{i}.test/"), long.clone())]);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.len() <= LOG_MAX);
        assert!(text.contains("https://699.test/") && !text.contains("https://0.test/"));
        assert!(text.lines().all(|l| l.split('\t').count() == 4), "trimmed at a line boundary");
        let _ = std::fs::remove_dir_all(dir);
    }
    use api::tests::{items, server};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cce-raindrop-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_first_pass_imports_creates_and_saves_the_base() {
        let dir = scratch("first");
        let bookmarks = Bookmarks::at(dir.join("bookmarks.tsv"));
        bookmarks.toggle("https://local.test/", "Mine");
        // Raindrop: 1.test and 2.test; then the create for local.test.
        let (base, seen) = server(vec![
            (200, "", items(1..3, 2)),
            (200, "", r#"{"result":true,"item":{"_id":90}}"#.into()),
        ]);
        let client = api::Client::with_base(Secret::from("t".to_string()), &base);
        let base_path = dir.join("raindrop-sync.tsv");
        let out = run_pass(&client, &bookmarks, &base_path, false).unwrap();
        let Outcome::Synced(s) = out else { panic!("refused") };
        assert_eq!((s.added_here, s.added_there), (2, 1));
        assert_eq!(s.text(), "synced — 2 added here, 1 added to Raindrop");
        let urls: Vec<_> = bookmarks.rows().into_iter().map(|r| r.1).collect();
        assert_eq!(urls.len(), 3);
        let saved = load_base(&base_path);
        assert_eq!(saved.len(), 3, "both imports and the create are paired");
        assert!(saved.iter().any(|b| b.id == 90 && b.url == "https://local.test/"));
        assert_eq!(seen.lock().unwrap().len(), 2);
        // The log names each bookmark that moved, and what happened to it.
        let log = std::fs::read_to_string(log_path(&base_path)).unwrap();
        let lines: Vec<Vec<&str>> = log.lines().map(|l| l.split('\t').collect()).collect();
        assert_eq!(lines.len(), 3, "{log}");
        assert!(lines.iter().all(|l| l.len() == 4 && l[0].ends_with('Z')));
        assert!(lines.iter().any(|l| l[1] == "added here" && l[2] == "https://1.test/" && l[3] == "t1"));
        assert!(lines.iter().any(|l| l[1] == "added to Raindrop" && l[2] == "https://local.test/" && l[3] == "Mine"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_pass_changes_nothing_until_forced() {
        let dir = scratch("refused");
        let bookmarks = Bookmarks::at(dir.join("bookmarks.tsv"));
        let base_path = dir.join("raindrop-sync.tsv");
        // Synced before: three bookmarks, all gone here now.
        save_base(&base_path, &(1..4).map(|i| super::super::Synced {
            id: i, url: format!("https://{i}.test/"), title: format!("t{i}"),
        }).collect::<Vec<_>>()).unwrap();
        let (base, seen) = server(vec![
            (200, "", items(1..4, 3)),
            (200, "", items(1..4, 3)),
            (200, "", "{}".into()),
            (200, "", "{}".into()),
            (200, "", "{}".into()),
        ]);
        let client = api::Client::with_base(Secret::from("t".to_string()), &base);
        let out = run_pass(&client, &bookmarks, &base_path, false).unwrap();
        assert!(matches!(out, Outcome::Refused(ref r) if r.contains("every bookmark in Raindrop")), "{out:?}");
        assert_eq!(seen.lock().unwrap().len(), 1, "only the fetch went out");
        assert_eq!(load_base(&base_path).len(), 3, "the base is untouched");
        assert!(std::fs::read_to_string(log_path(&base_path)).unwrap().contains("\trefused\t\tthis would delete every bookmark in Raindrop"));

        let Outcome::Synced(s) = run_pass(&client, &bookmarks, &base_path, true).unwrap() else {
            panic!("forced pass refused")
        };
        assert_eq!(s.trashed_there, 3);
        assert!(load_base(&base_path).is_empty());
        let log = std::fs::read_to_string(log_path(&base_path)).unwrap();
        assert_eq!(log.matches("\tmoved to Raindrop's trash\thttps://").count(), 3, "{log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_idle_pass_does_not_touch_the_file() {
        let dir = scratch("idle");
        let path = dir.join("bookmarks.tsv");
        let bookmarks = Bookmarks::at(path.clone());
        let (base, _) = server(vec![(200, "", items(1..1, 0))]);
        let client = api::Client::with_base(Secret::from("t".to_string()), &base);
        let out = run_pass(&client, &bookmarks, &dir.join("raindrop-sync.tsv"), false).unwrap();
        assert_eq!(out, Outcome::Synced(Summary::default()));
        assert_eq!(Summary::default().text(), "in sync");
        assert!(!path.exists(), "nothing to write, nothing written");
        assert!(!log_path(&dir.join("raindrop-sync.tsv")).exists(), "an idle pass logs nothing");
        let _ = std::fs::remove_dir_all(dir);
    }
}
