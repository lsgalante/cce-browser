# Bookmark sync with Raindrop.io

Status: **phase 3 done** (2026-10-02) — the browser syncs on its own when
`browser.raindrop` is on. Phase 1 is the merge (`src/raindrop/mod.rs`), phase 2
the REST client, keyring token and `cce-browser --raindrop-plan` dry run
(`src/raindrop/api.rs`), phase 3 the worker and the status line
(`src/raindrop/sync.rs`). 30 unit tests, plus one for the page links; phase 3 was also run end to end in a
shadow session against a stand-in Raindrop and a throwaway keyring.

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

## Phase 3 — the worker (done)

`browser.raindrop` (KDL, `browser { raindrop (bool)true }`, off by default) is
read at launch and on every focus like the other settings; the worker thread
`cce-raindrop` starts the first time it is on and idles while it is off. Its
writing side is the "Sync Bookmarks with Raindrop" toggle on
cce-system-interface's Browser page (`src/pages/browser.rs` there) — keep the
key name in step with `settings.rs`. Choices:

- **It polls the bookmarks every 2 s instead of being told about edits.** A
  bookmark changes from the star, Ctrl+D, the bookmarks menu and the
  `cce://bookmarks` page; comparing the in-memory rows (no I/O) catches all of
  them with no hook in each. A change syncs once it has held still for one
  poll, so a burst of edits is one pass. A full pass every 10 minutes brings
  Raindrop's side; the page's "sync now" asks for one at once.
- **A pass's local half is one locked edit** (`Bookmarks::edit_rows`), so no
  star or remove can land in the middle of it, and it is skipped entirely when
  the plan changes nothing here — an idle pass never rewrites the file. The
  worker re-reads the rows after its own pass, so its edits are not mistaken
  for the person's.
- **Titles are flattened where they arrive** (`api::parse_item`): a tab or a
  line break in a Raindrop title would otherwise be flattened by the TSV store
  and read as "renamed in Raindrop" on every pass.
- **A skipped link edit leaves the base** (`settle_base`): otherwise its pair
  would point at a URL that is not here and the next pass would trash
  Raindrop's copy.
- **The status line lives on `cce://bookmarks`**, set by the worker through
  `Bookmarks::set_sync_note`: what the last pass did and when, with "sync now".
  A refused pass shows why and a **"sync anyway"** link carrying a random code
  that `cce://bookmarks/sync-force` checks — a web page linking to `cce://`
  cannot force a mass deletion, since it cannot read the page to learn the
  code. The code holds for as long as passes keep being refused (a periodic
  pass, a reload); a fresh one each time made the link on screen stale, which
  is how the end-to-end run found it. After a forced pass the code is gone, so
  reloading its URL forces nothing.
- The page is static: the line is as of the page's load, and reloading
  `cce://bookmarks/sync` asks for another (harmless) pass.

**Testing without an account:** `CCE_RAINDROP_API=<base url>` points the
client at a stand-in (it logs a warning when it does). The end-to-end run used
a ~60-line Python stand-in for Unsorted (GET/POST/PUT/DELETE, held in memory),
a throwaway keyring holding a fake token (the autofill section of CLAUDE.md has
the recipe and its two traps), and checked: first pass imports and creates; a
local remove trashes; a rename and a save on the "phone" arrive; an emptied
collection is refused with the bookmarks untouched; "sync anyway" runs it.

## Phase 4 — favorites (optional)

A dedicated collection, ordered by Raindrop's manual sort.
