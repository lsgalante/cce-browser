//! Bookmark sync with Raindrop.io — phase 1: the merge, with no network.
//!
//! The design is in `RAINDROP-SYNC.md`. In short: the local `bookmarks.tsv`
//! stays the live store the chrome reads, and a sync pass reconciles it with
//! one Raindrop collection by a **three-way merge** against `base` — the set
//! as it stood after the last successful sync, kept in `raindrop-sync.tsv`.
//! Only a base can tell "deleted over there" from "new over here".
//!
//! Everything in this file is a pure function of its inputs, because that is
//! where a sync goes wrong: the API client (phase 2) only fetches and sends,
//! and the browser (phase 3) only applies. Rules the merge keeps:
//!
//! * **Identity is the Raindrop `_id`, not the URL.** A base entry pairs a
//!   local URL with a remote id; Raindrop may tidy a link, and keying on it
//!   would read every tidied link as one deletion plus one creation, forever.
//!   URLs pair only things the base has never seen (the first run, or the
//!   same page saved on both sides in between).
//! * **The browser owns the link and the title, nothing else.** A plan never
//!   carries tags, notes or collections, so it cannot clobber them.
//! * **A first run never deletes**: with an empty base nothing was ever
//!   synced, so nothing can have been deleted since.
//! * **Mass deletions are refused** (`guard`): an emptied file, the wrong
//!   collection or an API answer that came back empty all look like "delete
//!   everything", and that must take a person to confirm.
//! * **Raindrop's duplicates are left alone.** Locally a URL is a key; a second
//!   Raindrop entry for a link already here is neither imported nor removed.

pub mod api;
pub mod sync;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A Raindrop bookmark's id (`_id` in the API).
pub type RaindropId = u64;

/// A local bookmark: a row of `bookmarks.tsv`.
#[derive(Clone, Debug, PartialEq)]
pub struct Local {
    pub url: String,
    pub title: String,
    /// When it was bookmarked, in seconds — the local order.
    pub ts: u64,
}

/// A Raindrop bookmark, as much of it as the merge reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub id: RaindropId,
    pub link: String,
    pub title: String,
    /// `created`, in seconds — becomes `ts` when imported.
    pub created: u64,
}

/// One pair as it stood after the last successful sync.
#[derive(Clone, Debug, PartialEq)]
pub struct Synced {
    pub id: RaindropId,
    /// The local URL this id is paired with.
    pub url: String,
    /// The title both sides agreed on, which is what says *which side* changed
    /// it since.
    pub title: String,
}

/// What one sync pass will do. Applied in field order on each side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// New here: create in Raindrop (link and title only).
    pub create_remote: Vec<Local>,
    pub rename_remote: Vec<(RaindropId, String)>,
    /// Deleted here: move to Raindrop's trash, where it stays recoverable.
    pub trash_remote: Vec<RaindropId>,
    /// The link was edited in Raindrop: `(old local URL, new URL)`.
    pub relink_local: Vec<(String, String)>,
    /// `(local URL, new title)` — by the URL *after* any relink.
    pub rename_local: Vec<(String, String)>,
    pub delete_local: Vec<String>,
    /// New in Raindrop: add here.
    pub add_local: Vec<Local>,
    /// The base after this plan, except the pairs `create_remote` makes: their
    /// ids exist only once Raindrop answers. See [`Plan::base_after`].
    pub base: Vec<Synced>,
}

impl Plan {
    /// Whether this pass changes nothing on either side.
    pub fn is_noop(&self) -> bool {
        self.create_remote.is_empty()
            && self.rename_remote.is_empty()
            && self.trash_remote.is_empty()
            && self.relink_local.is_empty()
            && self.rename_local.is_empty()
            && self.delete_local.is_empty()
            && self.add_local.is_empty()
    }

    /// The base to save once the remote side has been applied (`prior` is the
    /// base the plan was made from). It must describe what *happened*, not
    /// what was planned, or the next pass misreads a failure:
    ///
    /// * a create that failed stays out of the base, so it is tried again as
    ///   "new here" — never read as deleted in Raindrop;
    /// * a trash that failed keeps its old pair, so it is tried again — not
    ///   re-imported as "new in Raindrop";
    /// * a rename that failed keeps its old title, so Raindrop's unchanged
    ///   title is not read as Raindrop renaming it back.
    pub fn base_after(&self, prior: &[Synced], applied: &api::Applied) -> Vec<Synced> {
        let mut base = self.base.clone();
        for id in &applied.failed_renames {
            if let (Some(b), Some(old)) =
                (base.iter_mut().find(|b| b.id == *id), prior.iter().find(|p| p.id == *id))
            {
                b.title = old.title.clone();
            }
        }
        for id in &applied.failed_trash {
            if let Some(old) = prior.iter().find(|p| p.id == *id) {
                base.push(old.clone());
            }
        }
        for (url, id) in &applied.created {
            if let Some(l) = self.create_remote.iter().find(|l| l.url == *url) {
                base.push(Synced { id: *id, url: l.url.clone(), title: l.title.clone() });
            }
        }
        base
    }
}

/// A pass refused by [`guard`], with the plan it would have run, so a person
/// can look at it and force it.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub reason: String,
    pub plan: Plan,
}

/// More deletions than this on one side in one pass needs a person.
pub const MAX_DELETES: usize = 10;
/// A side this large being emptied outright needs a person.
pub const WIPE_MIN: usize = 3;

/// The key two URLs are paired on when the base does not know them: scheme
/// and host case-folded (the URL parser does that) and one trailing slash
/// dropped. Only http(s) syncs — Raindrop stores web links, and a `file:`
/// bookmark means nothing on another machine. Not an identity: once paired,
/// the id is.
pub fn pair_key(url: &str) -> Option<String> {
    let u = url::Url::parse(url.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let mut s = u.to_string();
    if s.ends_with('/') {
        s.pop();
    }
    Some(s)
}

/// Work out one pass. `Err` when the pass trips the deletion guard.
pub fn plan(local: &[Local], remote: &[Remote], base: &[Synced]) -> Result<Plan, Refusal> {
    let mut plan = Plan::default();
    let local_by_url: HashMap<&str, &Local> = local.iter().map(|l| (l.url.as_str(), l)).collect();
    let remote_by_id: HashMap<RaindropId, &Remote> = remote.iter().map(|r| (r.id, r)).collect();
    // Every local URL's pairing key, so nothing imported duplicates one.
    let local_keys: HashSet<String> = local.iter().filter_map(|l| pair_key(&l.url)).collect();
    let mut used_local: HashSet<&str> = HashSet::new();
    let mut used_remote: HashSet<RaindropId> = HashSet::new();

    // 1. What the base knows: three-way.
    for b in base {
        let l = local_by_url.get(b.url.as_str()).copied();
        let r = remote_by_id.get(&b.id).copied();
        if let Some(l) = l {
            used_local.insert(l.url.as_str());
        }
        if let Some(r) = r {
            used_remote.insert(r.id);
        }
        match (l, r) {
            (Some(l), Some(r)) => {
                // Raindrop's link edited since: follow it, unless the new
                // link is already a bookmark here — then the pair stays put.
                let mut url = l.url.clone();
                if pair_key(&r.link) != pair_key(&l.url) {
                    match pair_key(&r.link) {
                        Some(k) if !local_keys.contains(&k) => {
                            plan.relink_local.push((l.url.clone(), r.link.clone()));
                            url = r.link.clone();
                        }
                        _ => {}
                    }
                }
                // Title: whichever side moved off the base wins; both moved,
                // Raindrop wins (the browser has no rename for bookmarks, so
                // a local change only comes from a hand-edited file).
                let title = if r.title != b.title {
                    if l.title != r.title {
                        plan.rename_local.push((url.clone(), r.title.clone()));
                    }
                    r.title.clone()
                } else {
                    if l.title != b.title {
                        plan.rename_remote.push((r.id, l.title.clone()));
                    }
                    l.title.clone()
                };
                plan.base.push(Synced { id: r.id, url, title });
            }
            // Gone from the collection — deleted there, or moved out of it.
            (Some(l), None) => plan.delete_local.push(l.url.clone()),
            (None, Some(r)) => plan.trash_remote.push(r.id),
            // Gone on both sides: nothing to do, and it leaves the base.
            (None, None) => {}
        }
    }

    // 2. What it does not: pair by URL, else it is new on its own side.
    let mut unpaired_remote: Vec<(String, &Remote)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in remote.iter().filter(|r| !used_remote.contains(&r.id)) {
        if let Some(k) = pair_key(&r.link) {
            // The first of Raindrop's duplicates speaks for the link.
            if seen.insert(k.clone()) {
                unpaired_remote.push((k, r));
            }
        }
    }
    let mut taken: HashSet<RaindropId> = HashSet::new();
    for l in local.iter().filter(|l| !used_local.contains(l.url.as_str())) {
        let Some(k) = pair_key(&l.url) else { continue };
        match unpaired_remote.iter().find(|(rk, r)| *rk == k && !taken.contains(&r.id)) {
            Some((_, r)) => {
                // Paired for the first time: Raindrop's title wins, since
                // that is where titles get edited.
                taken.insert(r.id);
                if l.title != r.title {
                    plan.rename_local.push((l.url.clone(), r.title.clone()));
                }
                plan.base.push(Synced { id: r.id, url: l.url.clone(), title: r.title.clone() });
            }
            None => plan.create_remote.push(l.clone()),
        }
    }
    // A link being deleted here this pass is free again: Raindrop's entry
    // for it is a re-save (deleted and saved anew there), not a duplicate.
    let leaving: HashSet<String> = plan.delete_local.iter().filter_map(|u| pair_key(u)).collect();
    for (k, r) in &unpaired_remote {
        if taken.contains(&r.id) || (local_keys.contains(k) && !leaving.contains(k)) {
            continue;
        }
        plan.add_local.push(Local { url: r.link.clone(), title: r.title.clone(), ts: r.created });
        plan.base.push(Synced { id: r.id, url: r.link.clone(), title: r.title.clone() });
    }

    match guard(&plan, local.len(), remote.len(), base.len()) {
        Some(reason) => Err(Refusal { reason, plan }),
        None => Ok(plan),
    }
}

/// Why a plan must not run unattended, if it must not.
pub fn guard(plan: &Plan, local_len: usize, remote_len: usize, base_len: usize) -> Option<String> {
    if remote_len == 0 && base_len > 0 {
        return Some(format!(
            "Raindrop returned no bookmarks, but {base_len} were synced before — \
             the wrong collection, or a token for another account?"
        ));
    }
    for (side, deletes, size) in [
        ("here", plan.delete_local.len(), local_len),
        ("in Raindrop", plan.trash_remote.len(), remote_len),
    ] {
        if deletes > MAX_DELETES {
            return Some(format!("this would delete {deletes} bookmarks {side} at once"));
        }
        if size >= WIPE_MIN && deletes == size {
            return Some(format!("this would delete every bookmark {side} ({size})"));
        }
    }
    None
}

/// What [`apply_local`] left undone.
#[derive(Debug, Default, PartialEq)]
pub struct Skipped {
    pub count: usize,
    /// The *new* URLs of link edits not applied. Their pairs must leave the
    /// base: it would pair the id with a URL that is not here, and the next
    /// pass would read that as "deleted here" and trash the Raindrop copy.
    /// Dropped from the base, the two simply re-pair (or both survive).
    pub relinks: Vec<String>,
}

/// Apply a plan's local half to the bookmarks as they are **now**.
///
/// The plan was made from a `snapshot`, and the person may have bookmarked or
/// removed something while Raindrop was answering. An operation on a URL whose
/// entry is not what the snapshot had is skipped — the next pass sees the new
/// state and plans again — so a local edit is never overwritten by a stale
/// plan.
pub fn apply_local(current: &mut Vec<Local>, snapshot: &[Local], plan: &Plan) -> Skipped {
    let unchanged = |current: &Vec<Local>, url: &str| {
        current.iter().find(|l| l.url == url) == snapshot.iter().find(|l| l.url == url)
    };
    let mut skipped = 0;
    let mut relinks = Vec::new();
    for (old, new) in &plan.relink_local {
        if unchanged(current, old) && !current.iter().any(|l| l.url == *new) {
            if let Some(l) = current.iter_mut().find(|l| l.url == *old) {
                l.url = new.clone();
            }
        } else {
            skipped += 1;
            relinks.push(new.clone());
        }
    }
    for (url, title) in &plan.rename_local {
        // Renames address the post-relink URL; a relinked entry was checked
        // against the snapshot by its old one above.
        let relinked =
            plan.relink_local.iter().any(|(_, n)| n == url) && !relinks.contains(url);
        if relinked || unchanged(current, url) {
            if let Some(l) = current.iter_mut().find(|l| l.url == *url) {
                l.title = title.clone();
                continue;
            }
        }
        skipped += 1;
    }
    for url in &plan.delete_local {
        if unchanged(current, url) {
            current.retain(|l| l.url != *url);
        } else {
            skipped += 1;
        }
    }
    for add in &plan.add_local {
        if current.iter().any(|l| l.url == add.url) {
            skipped += 1;
        } else {
            current.push(add.clone());
        }
    }
    // Local order is bookmarking time; an import lands where it was made.
    current.sort_by_key(|l| l.ts);
    Skipped { count: skipped, relinks }
}

/// The base to save after a pass: `base_after`, minus the pairs of link edits
/// that were skipped here (see [`Skipped::relinks`]).
pub fn settle_base(plan: &Plan, prior: &[Synced], applied: &api::Applied, skipped: &Skipped) -> Vec<Synced> {
    let mut base = plan.base_after(prior, applied);
    base.retain(|b| !skipped.relinks.contains(&b.url));
    base
}

/// `cce-browser --raindrop-plan`: fetch Unsorted, plan a pass against the
/// local bookmarks and the base, and describe it — changing nothing on either
/// side. The way to look at a real account before anything is allowed to
/// write to it.
pub fn dry_run() -> Result<String, String> {
    let local: Vec<Local> = crate::pages::Bookmarks::load()
        .rows()
        .into_iter()
        .map(|(ts, url, title)| Local { url, title, ts })
        .collect();
    let base = load_base(&state_path());
    let token = crate::accounts::raindrop_token()?;
    let remote = api::Client::new(token).fetch(api::UNSORTED).map_err(|e| e.to_string())?;
    let mut out = String::from("dry run: nothing has been changed\n\n");
    match plan(&local, &remote, &base) {
        Ok(p) => out.push_str(&api::describe(&p, &local, &remote, &base)),
        Err(refused) => {
            out.push_str(&format!("REFUSED: {}\n\n", refused.reason));
            out.push_str(&api::describe(&refused.plan, &local, &remote, &base));
        }
    }
    Ok(out)
}

/// `~/.local/state/cce/browser/raindrop-sync.tsv`.
pub fn state_path() -> PathBuf {
    crate::pages::state_dir().join("raindrop-sync.tsv")
}

/// Read the base: `id \t url \t title` per line. A malformed line is dropped,
/// and so is a repeat of an id or a URL — a pair is one-to-one, and a second
/// claim on either half can only be damage.
pub fn load_base(path: &Path) -> Vec<Synced> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(id), Some(url), Some(title)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(id) = id.parse::<RaindropId>() else { continue };
        if url.is_empty() || !ids.insert(id) || !urls.insert(url.to_string()) {
            continue;
        }
        out.push(Synced { id, url: url.to_string(), title: title.to_string() });
    }
    out
}

/// Write the base, atomically: a pass that dies mid-write must leave the old
/// base, not half a new one — half a base reads as half the bookmarks deleted.
pub fn save_base(path: &Path, base: &[Synced]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let field = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = String::new();
    for b in base {
        out.push_str(&format!("{}\t{}\t{}\n", b.id, field(&b.url), field(&b.title)));
    }
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(url: &str, title: &str, ts: u64) -> Local {
        Local { url: url.into(), title: title.into(), ts }
    }
    fn r(id: RaindropId, link: &str, title: &str) -> Remote {
        Remote { id, link: link.into(), title: title.into(), created: 1000 + id }
    }
    fn s(id: RaindropId, url: &str, title: &str) -> Synced {
        Synced { id, url: url.into(), title: title.into() }
    }

    /// Run a whole pass the way phases 2 and 3 will: plan, apply to Raindrop
    /// (handing out ids), apply locally, save the base. Returns the new state.
    fn run(
        local: &[Local],
        remote: &[Remote],
        base: &[Synced],
    ) -> (Vec<Local>, Vec<Remote>, Vec<Synced>) {
        let p = plan(local, remote, base).expect("not refused");
        let mut remote = remote.to_vec();
        remote.retain(|x| !p.trash_remote.contains(&x.id));
        for (id, t) in &p.rename_remote {
            remote.iter_mut().find(|x| x.id == *id).unwrap().title = t.clone();
        }
        let mut created = Vec::new();
        for c in &p.create_remote {
            let id = 500 + remote.len() as RaindropId + created.len() as RaindropId;
            remote.push(Remote { id, link: c.url.clone(), title: c.title.clone(), created: c.ts });
            created.push((c.url.clone(), id));
        }
        let mut local_now = local.to_vec();
        assert_eq!(apply_local(&mut local_now, local, &p).count, 0);
        let applied = api::Applied { created, ..Default::default() };
        (local_now, remote, p.base_after(base, &applied))
    }

    #[test]
    fn a_first_run_unions_and_pairs_by_link() {
        let local = [l("https://Example.com/", "mine", 1), l("https://only-here.test/a", "A", 2)];
        let remote = [r(1, "https://example.com", "theirs"), r(2, "https://only-there.test/", "B")];
        let p = plan(&local, &remote, &[]).unwrap();
        assert!(p.delete_local.is_empty() && p.trash_remote.is_empty(), "a first run never deletes");
        assert_eq!(p.rename_local, vec![("https://Example.com/".into(), "theirs".into())]);
        assert_eq!(p.create_remote, vec![l("https://only-here.test/a", "A", 2)]);
        assert_eq!(p.add_local, vec![l("https://only-there.test/", "B", 1002)]);
        assert!(p.base.contains(&s(1, "https://Example.com/", "theirs")));
    }

    #[test]
    fn deletions_cross_over_through_the_base() {
        let base = [s(1, "https://a.test/", "A"), s(2, "https://b.test/", "B")];
        // a.test deleted here, b.test deleted in Raindrop.
        let local = [l("https://b.test/", "B", 1)];
        let remote = [r(1, "https://a.test/", "A")];
        let p = plan(&local, &remote, &base).unwrap();
        assert_eq!(p.trash_remote, vec![1]);
        assert_eq!(p.delete_local, vec!["https://b.test/".to_string()]);
        assert!(p.base.is_empty());
        assert!(p.add_local.is_empty() && p.create_remote.is_empty(), "no resurrection");
    }

    #[test]
    fn titles_follow_whichever_side_moved() {
        let base = [s(1, "https://a.test/", "A"), s(2, "https://b.test/", "B"), s(3, "https://c.test/", "C")];
        let local = [l("https://a.test/", "A", 1), l("https://b.test/", "B local", 2), l("https://c.test/", "C local", 3)];
        let remote = [r(1, "https://a.test/", "A remote"), r(2, "https://b.test/", "B"), r(3, "https://c.test/", "C remote")];
        let p = plan(&local, &remote, &base).unwrap();
        assert_eq!(
            p.rename_local,
            vec![("https://a.test/".into(), "A remote".into()), ("https://c.test/".into(), "C remote".into())],
            "Raindrop's edit lands here, and wins when both moved"
        );
        assert_eq!(p.rename_remote, vec![(2, "B local".into())]);
    }

    #[test]
    fn a_link_edited_in_raindrop_is_followed() {
        let base = [s(1, "http://old.test/page", "P")];
        let local = [l("http://old.test/page", "P", 1)];
        let remote = [r(1, "https://new.test/page", "P")];
        let p = plan(&local, &remote, &base).unwrap();
        assert_eq!(p.relink_local, vec![("http://old.test/page".into(), "https://new.test/page".into())]);
        assert!(p.delete_local.is_empty() && p.add_local.is_empty());
        assert_eq!(p.base, vec![s(1, "https://new.test/page", "P")]);
    }

    #[test]
    fn a_tidied_link_is_not_a_change() {
        let base = [s(1, "https://Example.com/x/", "X")];
        let local = [l("https://Example.com/x/", "X", 1)];
        let remote = [r(1, "https://example.com/x", "X")];
        assert!(plan(&local, &remote, &base).unwrap().is_noop());
    }

    #[test]
    fn raindrop_duplicates_are_left_alone() {
        let base = [s(1, "https://a.test/", "A")];
        let local = [l("https://a.test/", "A", 1)];
        let remote = [r(1, "https://a.test/", "A"), r(2, "https://a.test", "A again"), r(3, "https://b.test/", "B"), r(4, "https://b.test/", "B again")];
        let p = plan(&local, &remote, &base).unwrap();
        assert_eq!(p.add_local, vec![l("https://b.test/", "B", 1003)], "one import per link");
        assert!(p.trash_remote.is_empty(), "and nothing of theirs is removed");
    }

    #[test]
    fn a_link_re_saved_in_raindrop_settles_in_one_pass() {
        // Deleted and saved again in Raindrop: same link, a new id.
        let base = [s(1, "https://a.test/", "A")];
        let local = [l("https://a.test/", "A", 1)];
        let remote = [r(7, "https://a.test/", "A anew")];
        let (local, remote, base) = run(&local, &remote, &base);
        assert_eq!(local, vec![l("https://a.test/", "A anew", 1007)]);
        assert!(plan(&local, &remote, &base).unwrap().is_noop());
    }

    #[test]
    fn only_web_links_sync() {
        let local = [l("file:///home/me/notes.html", "notes", 1)];
        let p = plan(&local, &[r(1, "https://a.test/", "A")], &[]).unwrap();
        assert!(p.create_remote.is_empty(), "a file: bookmark stays here");
    }

    #[test]
    fn mass_deletion_is_refused() {
        let n = MAX_DELETES as RaindropId + 1;
        let base: Vec<_> = (1..=n).map(|i| s(i, &format!("https://{i}.test/"), "t")).collect();
        let mut remote: Vec<_> = (1..=n).map(|i| r(i, &format!("https://{i}.test/"), "t")).collect();
        remote.push(r(99, "https://keep.test/", "k"));
        let err = plan(&[], &remote, &base).unwrap_err();
        assert!(err.reason.contains("11 bookmarks in Raindrop"), "{}", err.reason);
        assert_eq!(err.plan.trash_remote.len(), 11, "the refused plan is kept for a person to force");
    }

    #[test]
    fn emptying_a_side_is_refused() {
        let base: Vec<_> = (1..=3).map(|i| s(i, &format!("https://{i}.test/"), "t")).collect();
        let local: Vec<_> = (1..=3).map(|i| l(&format!("https://{i}.test/"), "t", i)).collect();
        let remote: Vec<_> = (1..=3).map(|i| r(i, &format!("https://{i}.test/"), "t")).collect();
        assert!(plan(&[], &remote, &base).unwrap_err().reason.contains("every bookmark in Raindrop"));
        assert!(plan(&local, &[], &base).unwrap_err().reason.contains("returned no bookmarks"));
    }

    #[test]
    fn ordinary_deletion_is_allowed() {
        let base = [s(1, "https://a.test/", "A"), s(2, "https://b.test/", "B")];
        let local = [l("https://a.test/", "A", 1)];
        let remote = [r(1, "https://a.test/", "A"), r(2, "https://b.test/", "B")];
        assert_eq!(plan(&local, &remote, &base).unwrap().trash_remote, vec![2]);
        // Deleting the only bookmark is a person's choice, not a wipe.
        let p = plan(&[], &[r(1, "https://a.test/", "A")], &[s(1, "https://a.test/", "A")]).unwrap();
        assert_eq!(p.trash_remote, vec![1]);
    }

    #[test]
    fn a_pass_settles_and_the_next_one_is_a_noop() {
        let local = vec![l("https://a.test/", "A", 1), l("https://here.test/", "H", 2)];
        let remote = vec![r(1, "https://a.test", "A (theirs)"), r(2, "https://there.test/", "T")];
        let (local, remote, base) = run(&local, &remote, &[]);
        assert_eq!(local.len(), 3);
        assert_eq!(remote.len(), 3);
        assert!(plan(&local, &remote, &base).unwrap().is_noop(), "a synced state plans nothing");

        // A deletion on each side, then a rename in Raindrop, each settles.
        let local: Vec<_> = local.into_iter().filter(|x| x.url != "https://here.test/").collect();
        let (local, mut remote, base) = run(&local, &remote, &base);
        assert_eq!(remote.len(), 2);
        remote.iter_mut().find(|x| x.link.contains("there")).unwrap().title = "T2".into();
        let (local, remote, base) = run(&local, &remote, &base);
        assert!(local.iter().any(|x| x.title == "T2"));
        assert!(plan(&local, &remote, &base).unwrap().is_noop());
    }

    #[test]
    fn a_failed_create_is_retried_not_deleted() {
        let local = [l("https://new.test/", "N", 1)];
        let p = plan(&local, &[], &[]).unwrap();
        let base = p.base_after(&[], &api::Applied::default()); // Raindrop refused the create
        let again = plan(&local, &[], &base).unwrap();
        assert_eq!(again.create_remote.len(), 1);
        assert!(again.delete_local.is_empty());
    }

    #[test]
    fn failed_trash_and_rename_are_retried_not_reversed() {
        let prior = [s(1, "https://a.test/", "A"), s(2, "https://b.test/", "B")];
        // a.test deleted here; b.test renamed here.
        let local = [l("https://b.test/", "B new", 2)];
        let remote = [r(1, "https://a.test/", "A"), r(2, "https://b.test/", "B")];
        let p = plan(&local, &remote, &prior).unwrap();
        assert_eq!((p.trash_remote.clone(), p.rename_remote.clone()), (vec![1], vec![(2, "B new".to_string())]));
        // Both calls fail.
        let applied = api::Applied { failed_trash: vec![1], failed_renames: vec![2], ..Default::default() };
        let base = p.base_after(&prior, &applied);
        let again = plan(&local, &remote, &base).unwrap();
        assert_eq!(again.trash_remote, vec![1], "the trash is retried");
        assert!(again.add_local.is_empty(), "not re-imported");
        assert_eq!(again.rename_remote, vec![(2, "B new".to_string())], "the rename is retried");
        assert!(again.rename_local.is_empty(), "not reversed");
    }

    #[test]
    fn edits_made_during_a_pass_survive_it() {
        let snapshot = vec![l("https://a.test/", "A", 1), l("https://b.test/", "B", 2)];
        let p = Plan {
            delete_local: vec!["https://a.test/".into()],
            rename_local: vec![("https://b.test/".into(), "B2".into())],
            add_local: vec![l("https://c.test/", "C", 3)],
            ..Plan::default()
        };
        // Meanwhile: a.test re-bookmarked with a new title, c.test bookmarked.
        let mut current = vec![l("https://a.test/", "A again", 5), l("https://b.test/", "B", 2), l("https://c.test/", "mine", 6)];
        assert_eq!(apply_local(&mut current, &snapshot, &p).count, 2);
        assert!(current.iter().any(|x| x.url == "https://a.test/" && x.title == "A again"));
        assert!(current.iter().any(|x| x.url == "https://b.test/" && x.title == "B2"));
        assert!(current.iter().any(|x| x.url == "https://c.test/" && x.title == "mine"));
    }

    #[test]
    fn a_skipped_link_edit_cannot_become_a_deletion() {
        let prior = [s(1, "http://old.test/", "P")];
        let snapshot = vec![l("http://old.test/", "P", 1)];
        let remote = [r(1, "https://new.test/", "P")];
        let p = plan(&snapshot, &remote, &prior).unwrap();
        // Mid-pass the person re-bookmarked the old link with a new title.
        let mut current = vec![l("http://old.test/", "P again", 9)];
        let skipped = apply_local(&mut current, &snapshot, &p);
        assert_eq!(skipped.relinks, vec!["https://new.test/".to_string()]);
        let base = settle_base(&p, &prior, &api::Applied::default(), &skipped);
        let next = plan(&current, &remote, &base).unwrap();
        assert!(next.trash_remote.is_empty(), "Raindrop's copy is not trashed");
        assert!(next.delete_local.is_empty(), "and neither is the local one");
    }

    #[test]
    fn the_base_round_trips_and_drops_damage() {
        let dir = std::env::temp_dir().join(format!("cce-raindrop-{}", std::process::id()));
        let path = dir.join("raindrop-sync.tsv");
        save_base(&path, &[s(1, "https://a.test/", "tab\there"), s(2, "https://b.test/", "B")]).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("3\thttps://a.test/\tsame url\n1\thttps://c.test/\tsame id\nnot a line\n");
        std::fs::write(&path, text).unwrap();
        assert_eq!(load_base(&path), vec![s(1, "https://a.test/", "tab here"), s(2, "https://b.test/", "B")]);
        assert!(!path.with_extension("tsv.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
