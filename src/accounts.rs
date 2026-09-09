//! Accounts from cce-secrets — the login suggestions the URL of a page earns.
//!
//! There is no cce-secrets *protocol*: that app fronts the freedesktop
//! **Secret Service** (gnome-keyring on this machine), and so does this. The
//! entry shape is the one cce-secrets writes and KeePassXC maps onto its own
//! fields: the item label is the title, and `UserName` / `URL` are ordinary
//! attributes beside it. Read the sibling crate's `CLAUDE.md` before changing
//! the attribute names here — both ends have to agree.
//!
//! Two rules shape everything below.
//!
//! **Secrets are fetched one at a time, at the moment of a pick.** Listing
//! reads labels, usernames and URLs only; no password is fetched to build a
//! menu, and none is held afterwards. [`Secret`] exists so that a password
//! cannot reach a log through a derived `Debug`.
//!
//! **The keyring is never touched on the frame path.** A locked collection
//! prompts, and a prompt blocks for as long as the person takes to answer it,
//! so all of it runs on a worker thread that talks back through the app's
//! calloop channel. That is also why this uses the *blocking* Secret Service
//! API: on its own thread, blocking is the simple correct thing, and it keeps
//! an async runtime out of the browser.

use std::sync::mpsc;

use crate::Message;

/// Attribute names to read a username from, in order of preference. cce-secrets
/// writes `UserName`; entries born elsewhere in the keyring use lowercase.
const USER_KEYS: [&str; 3] = ["UserName", "username", "user"];
/// Same, for the entry's site.
const URL_KEYS: [&str; 3] = ["URL", "url", "uri"];

/// A password on its way from the keyring to one page field.
///
/// The wrapper is the point: `Message` derives `Debug`, and a plain `String`
/// in it would put a live password into any log line that ever formats a
/// message. This one prints as `Secret(…)` and hands over its contents only
/// to a caller that asks for them by name.
#[derive(Clone, PartialEq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// One keyring entry as the chrome lists it — never its secret.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    /// Secret Service object path: the handle the secret is fetched by when
    /// this account is picked.
    pub path: String,
    /// The entry's title.
    pub label: String,
    pub username: String,
    /// The `URL` attribute as stored, empty when the entry has none.
    pub url: String,
}

impl Account {
    /// The host this entry claims, if any. Entries are written by people and
    /// by importers, so the field holds anything from a full URL to a bare
    /// domain; both have to work.
    pub fn host(&self) -> Option<String> {
        entry_host(&self.url)
    }

    /// Whether this entry is worth offering on `host`.
    ///
    /// Deliberately narrow. An exact host matches, and a *parent* domain
    /// matches its subdomains — an entry for `example.com` is offered on
    /// `login.example.com`, which is how sites actually split their login
    /// pages. The reverse is not true: an entry for `login.example.com` is
    /// not offered on `example.com`, and never on an unrelated host, because
    /// a suggestion is a request to hand a password to whatever is on screen.
    ///
    /// An entry with no URL at all falls back to its title: a KeePass entry
    /// called "GitHub" is offered on `github.com`. That one is a guess, so it
    /// is only made when there is nothing better to go on.
    pub fn matches(&self, host: &str) -> bool {
        let page = normalize_host(host);
        if page.is_empty() {
            return false;
        }
        match self.host() {
            Some(entry) => {
                page == entry || (entry.contains('.') && page.ends_with(&format!(".{entry}")))
            }
            None => {
                let title = self.label.trim().to_lowercase();
                !title.is_empty()
                    && registrable_label(&page).is_some_and(|name| name == title)
            }
        }
    }
}

/// Lowercase, and without the `www.` that no one means.
fn normalize_host(host: &str) -> String {
    let h = host.trim().to_lowercase();
    h.strip_prefix("www.").unwrap_or(&h).to_string()
}

/// The host inside a stored `URL` attribute: a real URL as written, a bare
/// host by guessing the scheme the same way the URL bar does.
pub fn entry_host(url: &str) -> Option<String> {
    let s = url.trim();
    if s.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(s)
        .ok()
        .or_else(|| url::Url::parse(&format!("https://{s}")).ok())?;
    let host = parsed.host_str()?;
    let host = normalize_host(host);
    (!host.is_empty()).then_some(host)
}

/// The name a site goes by: `github` out of `github.com`, `bbc` out of
/// `bbc.co.uk`. Not a public-suffix list — it exists only for the
/// title-matching fallback, where being roughly right is the whole ambition.
fn registrable_label(host: &str) -> Option<String> {
    let parts: Vec<&str> = host.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    // Two-letter final labels are country codes, where the name sits one
    // further left (co.uk, com.au) unless the domain is only two deep.
    let idx = if parts.len() >= 3 && parts[parts.len() - 1].len() == 2 && parts[parts.len() - 2].len() <= 3
    {
        parts.len() - 3
    } else {
        parts.len() - 2
    };
    Some(parts[idx].to_string())
}

/// What the worker is asked to do.
enum Request {
    /// Read every account the keyring holds (labels and attributes only).
    Load,
    /// Fetch one entry's password, by object path.
    Fetch(String),
}

/// The account index, and the thread that reads it.
///
/// Nothing here touches the keyring until something asks: the first login
/// field on the first page is what wakes it, so a browser that never sees a
/// login form never opens the store — and never triggers an unlock prompt at
/// launch, which is the behaviour that would have made this unwelcome.
pub struct Accounts {
    tx: mpsc::Sender<Request>,
    /// Every account the last load returned.
    all: Vec<Account>,
    /// A load has been asked for and not yet answered.
    loading: bool,
    /// Set once a load has come back, so an empty keyring is not retried on
    /// every focus.
    loaded: bool,
    /// Why the last load failed, for the menu to say so instead of showing
    /// an empty list that looks like "no accounts".
    pub error: Option<String>,
}

impl Accounts {
    /// Start the worker. It is idle until [`Accounts::ensure_loaded`].
    pub fn spawn(sender: calloop::channel::Sender<Message>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("cce-accounts".to_string())
            .spawn(move || worker(rx, sender))
            .expect("spawn the accounts worker");
        Self { tx, all: Vec::new(), loading: false, loaded: false, error: None }
    }

    /// Ask for the index if it is not already here or on its way.
    pub fn ensure_loaded(&mut self) {
        if self.loaded || self.loading {
            return;
        }
        self.loading = true;
        let _ = self.tx.send(Request::Load);
    }

    /// Take the worker's answer.
    pub fn loaded(&mut self, result: Result<Vec<Account>, String>) {
        self.loading = false;
        self.loaded = true;
        match result {
            Ok(all) => {
                self.all = all;
                self.error = None;
            }
            Err(e) => {
                self.all.clear();
                self.error = Some(e);
            }
        }
    }

    /// Fetch one password. It comes back as [`Message::Credential`].
    pub fn fetch(&self, path: &str) {
        let _ = self.tx.send(Request::Fetch(path.to_string()));
    }

    /// The accounts worth offering on `host`, best first: entries with a real
    /// URL ahead of ones matched by their title alone, then by label.
    pub fn matching(&self, host: &str) -> Vec<Account> {
        let mut hits: Vec<Account> =
            self.all.iter().filter(|a| a.matches(host)).cloned().collect();
        hits.sort_by(|a, b| {
            b.host()
                .is_some()
                .cmp(&a.host().is_some())
                .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
        });
        hits
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }
}

/// The worker thread: one Secret Service connection, held for the life of the
/// browser, serving requests in order.
fn worker(rx: mpsc::Receiver<Request>, tx: calloop::channel::Sender<Message>) {
    use secret_service::blocking::SecretService;
    use secret_service::EncryptionType;

    let mut service: Option<SecretService> = None;
    while let Ok(request) = rx.recv() {
        // Connect on the first request, and again after a failure — the
        // daemon can come and go.
        if service.is_none() {
            // Dh, not Plain: the secret then crosses the bus encrypted under a
            // session key rather than in the clear.
            match SecretService::connect(EncryptionType::Dh) {
                Ok(s) => service = Some(s),
                Err(e) => {
                    let _ = tx.send(Message::Accounts(Err(format!("no secret service: {e}"))));
                    continue;
                }
            }
        }
        let Some(ss) = service.as_ref() else { continue };
        match request {
            Request::Load => {
                let _ = tx.send(Message::Accounts(load(ss)));
            }
            Request::Fetch(path) => {
                if let Some(secret) = fetch(ss, &path) {
                    let _ = tx.send(Message::Credential(path, secret));
                }
            }
        }
    }
}

fn load(ss: &secret_service::blocking::SecretService) -> Result<Vec<Account>, String> {
    let collections = ss
        .get_all_collections()
        .map_err(|e| format!("listing collections failed: {e}"))?;
    let mut accounts = Vec::new();
    for collection in &collections {
        // A locked collection is skipped rather than unlocked: the browser
        // asking for the keyring password because a page happened to have a
        // login field would be its own kind of phishing lesson. cce-secrets
        // is the place to unlock.
        if collection.is_locked().unwrap_or(true) {
            continue;
        }
        let Ok(items) = collection.get_all_items() else { continue };
        for item in items {
            let Ok(attrs) = item.get_attributes() else { continue };
            let pick = |keys: &[&str]| -> String {
                keys.iter()
                    .find_map(|k| attrs.get(*k).filter(|v| !v.trim().is_empty()))
                    .cloned()
                    .unwrap_or_default()
            };
            let username = pick(&USER_KEYS);
            let url = pick(&URL_KEYS);
            // An entry with neither is not an account — a note, a key, a
            // token — and has nothing to offer a login form.
            if username.is_empty() && url.is_empty() {
                continue;
            }
            accounts.push(Account {
                path: item.item_path.to_string(),
                label: item.get_label().unwrap_or_default(),
                username,
                url,
            });
        }
    }
    Ok(accounts)
}

/// One entry's password. A failure is silent on purpose: the error text from
/// this call can carry the item's own label, and it has nowhere to go but a
/// log.
fn fetch(ss: &secret_service::blocking::SecretService, path: &str) -> Option<Secret> {
    let path = zbus::zvariant::OwnedObjectPath::try_from(path).ok()?;
    let item = ss.get_item_by_path(path).ok()?;
    let bytes = item.get_secret().ok()?;
    Some(Secret(String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(label: &str, url: &str) -> Account {
        Account {
            path: "/org/freedesktop/secrets/item/1".to_string(),
            label: label.to_string(),
            username: "me".to_string(),
            url: url.to_string(),
        }
    }

    #[test]
    fn a_stored_url_matches_its_own_host_and_its_subdomains() {
        let a = account("Example", "https://example.com/login?next=/");
        assert!(a.matches("example.com"));
        assert!(a.matches("www.example.com"), "www is not a different site");
        assert!(a.matches("login.example.com"), "a parent domain covers its subdomains");
        assert!(!a.matches("example.com.evil.test"), "suffix games are not matches");
        assert!(!a.matches("notexample.com"));
        assert!(!a.matches("example.org"));
    }

    #[test]
    fn a_subdomain_entry_does_not_leak_upward() {
        let a = account("Mail", "https://mail.example.com/");
        assert!(a.matches("mail.example.com"));
        assert!(!a.matches("example.com"), "the parent is a different site");
        assert!(!a.matches("chat.example.com"), "so is a sibling");
    }

    #[test]
    fn a_bare_host_is_a_url_too() {
        assert_eq!(entry_host("example.com"), Some("example.com".to_string()));
        assert_eq!(entry_host("https://WWW.Example.COM/x"), Some("example.com".to_string()));
        assert_eq!(entry_host("  "), None);
        assert_eq!(entry_host("not a url at all"), None);
    }

    #[test]
    fn an_entry_without_a_url_falls_back_to_its_title() {
        let a = account("GitHub", "");
        assert!(a.matches("github.com"));
        assert!(a.matches("gist.github.com"), "the site name is the same one");
        assert!(!a.matches("github.evil.test"));
        assert!(!a.matches("gitlab.com"));

        // The fallback is only for entries with nothing else to go on.
        let titled = account("GitHub", "https://example.com/");
        assert!(!titled.matches("github.com"), "a stored URL wins over the title");
    }

    #[test]
    fn country_code_domains_still_find_their_name() {
        assert_eq!(registrable_label("bbc.co.uk").as_deref(), Some("bbc"));
        assert_eq!(registrable_label("www.example.com").as_deref(), Some("example"));
        assert_eq!(registrable_label("localhost"), None);
    }

    #[test]
    fn a_password_never_prints_itself() {
        let s = Secret("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "Secret(…)");
        assert_eq!(format!("{:?}", Message::Credential("/p".into(), s.clone())),
                   "Credential(\"/p\", Secret(…))");
        assert_eq!(s.expose(), "hunter2");
    }
}
