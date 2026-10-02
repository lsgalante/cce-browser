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
//! line on `cce://bookmarks`.

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
    let plan = match plan(&snapshot, &remote, &base) {
        Ok(p) => p,
        Err(refused) if force => {
            log::warn!("raindrop: running a refused pass on request ({})", refused.reason);
            refused.plan
        }
        Err(refused) => return Ok(Outcome::Refused(refused.reason)),
    };
    let applied = api::apply_remote(client, api::UNSORTED, &plan).map_err(|e| e.to_string())?;
    let touches_local = !(plan.relink_local.is_empty()
        && plan.rename_local.is_empty()
        && plan.delete_local.is_empty()
        && plan.add_local.is_empty());
    // Only rewrite the file when there is something to write: a pass every
    // ten minutes that changes nothing must not touch the disk.
    let skipped = if touches_local {
        bookmarks.edit_rows(|rows| {
            let mut current = locals(std::mem::take(rows));
            let skipped = apply_local(&mut current, &snapshot, &plan);
            *rows = current.into_iter().map(|l| (l.ts, l.url, l.title)).collect();
            skipped
        })
    } else {
        super::Skipped::default()
    };
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

        let Outcome::Synced(s) = run_pass(&client, &bookmarks, &base_path, true).unwrap() else {
            panic!("forced pass refused")
        };
        assert_eq!(s.trashed_there, 3);
        assert!(load_base(&base_path).is_empty());
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
        let _ = std::fs::remove_dir_all(dir);
    }
}
