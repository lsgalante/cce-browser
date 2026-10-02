# Bookmark sync with Raindrop.io

Status: **phase 1 done** (2026-10-02) — the merge, the deletion guard, the sync
state file and applying a plan locally, all in `src/raindrop.rs` with no
network, under 15 unit tests. Nothing is wired into the browser yet.

## Decisions

- **One Raindrop collection — Unsorted (id `-1`) — mirrors the browser's
  bookmarks**, and new bookmarks from the browser land there. It is also where
  Raindrop's phone app and extensions save by default, so a bookmark saved
  anywhere shows up here. The cost, accepted: filing a bookmark into another
  collection in Raindrop moves it out of the mirror, and the next pass removes
  it here (not from Raindrop). Mirroring one collection and creating in another
  was ruled out — every bookmark made here would read as deleted there.
- **Deleting here moves the Raindrop copy to its trash** (recoverable there).
- **Favorites stay local.** A dedicated Raindrop collection is the upgrade path
  (phase 4), using its manual order for strip order.

## Shape

A worker thread **inside the browser**, not a separate tool like
cce-keyring-sync: only the browser edits bookmarks locally, and it holds them
in memory (`pages::Bookmarks`, rewriting `bookmarks.tsv` on each edit), so a
second process writing that file would race it. It syncs at launch, a few
seconds after a local edit (debounced), and every ~10 minutes while running.
The menu and `cce://bookmarks` keep reading the local store and never wait on
the network.

## The merge (phase 1, `src/raindrop.rs`)

`bookmarks.tsv` is unchanged. `raindrop-sync.tsv` holds the **base** — `id \t
local url \t title` per pair, as of the last successful pass — and `plan(local,
remote, base)` is a three-way merge:

| here | Raindrop | in base | action |
| --- | --- | --- | --- |
| ✓ | ✓ | ✓ | title: the side that moved off the base wins; both → Raindrop |
| ✓ | – | no | create in Raindrop |
| ✓ | – | ✓ | delete here (deleted there, or moved out of the collection) |
| – | ✓ | no | add here, at its `created` time |
| – | ✓ | ✓ | move to Raindrop's trash |

Rules, each with a test:

- **Identity is the Raindrop id.** URLs pair only what the base has never seen
  (`pair_key`: scheme/host case and a trailing slash don't matter). A link
  edited in Raindrop is followed locally (`relink_local`).
- **Only link and title are ever sent** — tags, notes and collections set
  elsewhere cannot be clobbered (the cce-secrets attribute-wipe lesson).
- **A first run never deletes** (empty base: union only).
- **The guard refuses** more than 10 deletions on a side, emptying a side of 3
  or more, or Raindrop answering empty when the base is not. The refused plan
  is returned so a person can force it.
- **Local edits made during a pass survive it**: `apply_local` skips any
  operation on an entry that changed since the snapshot the plan was made from.
- **A failed create retries next pass**, never reads as a deletion
  (`base_after` only records ids Raindrop actually returned).
- **The base is written atomically** (temp file + rename): half a base would
  read as half the bookmarks deleted.
- Raindrop's own duplicates are neither imported nor removed; `file:`
  bookmarks stay local.
- A pass settles: re-planning a synced state is a no-op (tested end to end).

## Phase 2 — the API client

To confirm against Raindrop's API docs first: REST at
`api.raindrop.io/rest/v1`; a personal **test token** (integration settings) as
`Authorization: Bearer`, so no OAuth; `GET /raindrops/{collection}` paged 50 at
a time (a full fetch each pass — ~20 requests per 1000 bookmarks, inside the
~120/min limit); `POST /raindrop` to create, `PUT /raindrop/{id}` to rename
(title only), `DELETE /raindrop/{id}` to trash. Blocking `reqwest` on the
worker (already a dependency). A dry-run mode logs the plan and applies
nothing — run it against the real account before anything writes.

**The token** lives in the keyring as an entry with no `UserName` (attribute
`service=raindrop.io`), so cce-keyring-sync — which skips entries without
`UserName` — never sends it to 1Password. A locked keyring skips the pass; it
never prompts.

## Phase 3 — wiring

The worker, the debounce, `apply_local` against `pages::Bookmarks` under its
lock, `save_base` last. A one-line status on `cce://bookmarks` ("synced 3m
ago", or why not, including a refused plan with a way to force it). Setting
`browser.raindrop` (off by default); its writing side belongs to
cce-system-interface's Browser page — keep the key names in sync.

## Phase 4 — favorites (optional)

A dedicated collection, ordered by Raindrop's manual sort.
