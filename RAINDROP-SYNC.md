# Bookmark sync with Raindrop.io

Status: **phase 2 done** (2026-10-02). Phase 1 is the merge, the deletion
guard, the sync state file and applying a plan locally (`src/raindrop/mod.rs`);
phase 2 is the REST client, the keyring token and a read-only dry run
(`src/raindrop/api.rs`, `cce-browser --raindrop-plan`). 25 unit tests, the
client's against a stand-in server. Nothing syncs on its own yet — phase 3.

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

## Phase 2 — the API client (done)

Checked against developer.raindrop.io on 2026-10-02: REST at
`api.raindrop.io/rest/v1`, `Authorization: Bearer <test token>` (from the
integration settings; test tokens do not expire), 120 requests/minute with
`429` past it, ISO 8601 timestamps. `GET /raindrops/-1` pages Unsorted 50 at a
time; `POST /raindrop` creates; `PUT /raindrop/{id}` is a **partial** update;
`DELETE /raindrop/{id}` moves to Trash — and is **permanent** on an item
already in Trash, so only ids just fetched from the live collection are ever
trashed. Choices:

- **The fetch is checked against Raindrop's `count`.** Paging is by position,
  so a deletion between pages shifts an item past the fetch, and a missing
  item reads as "deleted in Raindrop". Sorted oldest-first, an *addition*
  lands on the last page; a mismatch refuses the whole pass.
- **A failure on one item does not stop the rest**, and `base_after` records
  what *happened*: a failed create stays out of the base (retried as new), a
  failed trash keeps its pair (retried, not re-imported), a failed rename keeps
  its old title (retried, not reversed). A `401` stops the pass at once.
- **One `429` is waited out** (until `X-RateLimit-Reset`, at most a minute).
- `reqwest` is now a plain dependency (it was Servo-only) — `blocking`, the
  same build cce-map and cce-calendar use; JSON bodies are serialized by hand
  rather than turning on its `json` feature, to keep it the same build.

**The token** lives in the keyring as an entry with no `UserName`, found by
`service=raindrop.io`:

```sh
secret-tool store --label='Raindrop.io token' service raindrop.io
```

cce-keyring-sync skips entries without `UserName`, so it stays on this machine
and never goes to 1Password; the account index skips it too, so it is never
offered to a login form. A locked keyring is an error, never a prompt.

**`cce-browser --raindrop-plan`** fetches Unsorted, plans a pass against
`bookmarks.tsv` and the base, prints it, and changes nothing. It runs ahead of
the single-instance hand-off, so it works while the browser is open.

## Phase 3 — wiring

The worker, the debounce, `apply_local` against `pages::Bookmarks` under its
lock, `save_base` last. A one-line status on `cce://bookmarks` ("synced 3m
ago", or why not, including a refused plan with a way to force it). Setting
`browser.raindrop` (off by default); its writing side belongs to
cce-system-interface's Browser page — keep the key names in sync.

## Phase 4 — favorites (optional)

A dedicated collection, ordered by Raindrop's manual sort.
