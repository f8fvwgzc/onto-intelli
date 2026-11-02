//! External signals (docs/design/SIGNALS.md): news, RSS and Atom feeds
//! whose items are kept as events (`signal.item`) and knowledge documents,
//! never as entities, and a recommender that reads them against the graph
//! and suggests link types and chain rules. Suggestions become schema
//! proposals only when someone asks (`signals.propose`), and people review
//! and apply them through the proposal flow: nothing here changes the
//! schema or the graph.
//!
//! * A feed is registered by an admin (`signals.feed_add`) and fetched on
//!   demand (`signals.fetch`; a scheduler or an automation can call it).
//!   Fetching follows the SSRF rules of [`crate::net`]: `https` only, public
//!   addresses only (no operator allow rules), the resolver checks every
//!   answer before connecting, at most three redirects, each checked again,
//!   no proxy, a size and a time limit.
//! * Each new item (by its guid, link or title) becomes one `signal.item`
//!   event in the feed's workspace (`source` = `feed:<name>`), and, unless
//!   the feed says otherwise, a knowledge document in the collection
//!   `signals:<name>`, so `knowledge.search` finds it.
//! * The recommender finds the entities each item names (whole-word,
//!   case-insensitive matches of the titles and aliases of entities the
//!   caller may read) and
//!   counts entities named together. Kinds named together in at least
//!   `min_support` items, with no link type between them and no link
//!   between the entities, suggest a link type. Pairs that a two-step path
//!   of the same two relations joins suggest a chain rule inferring it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use onto_core::live_schema::{SchemaChange, SchemaChangeSet};
use onto_core::query::FindSpec;
use onto_core::schema::{LinkType, Rule};
use onto_core::time::now_ms;
use onto_core::{
    ActionContext, Entity, Error, EventFilter, EventInput, Mutation, Principal, ProposalInput, ProposalStatus, Result,
};

use crate::Intelli;
use crate::knowledge::{IngestInput, Sensitivity};

/// Registered feeds, by name.
pub const NS_FEED: &str = "signals.feed";
/// The action of the event each feed item becomes.
pub const ITEM_ACTION: &str = "signal.item";
/// Items' guids remembered per feed (newest kept).
const SEEN_MAX: usize = 5_000;
/// Bytes of a feed document read at most.
pub const MAX_FEED_BYTES: usize = 4 * 1024 * 1024;
/// Items read per fetch at most.
const MAX_ITEMS: usize = 500;
/// Characters of an item's summary kept in its event.
const EVENT_SUMMARY_CHARS: usize = 1_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Feed {
    pub name: String,
    pub url: String,
    pub workspace: String,
    /// Knowledge collection of its items (default `signals:<name>`).
    pub collection: String,
    /// Sensitivity of its items' documents (default `public`: news).
    #[serde(default)]
    pub sensitivity: Sensitivity,
    /// Also keep items as knowledge documents.
    #[serde(default = "yes")]
    pub knowledge: bool,
    pub created_by: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fetch: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Items stored so far.
    #[serde(default)]
    pub items: u64,
    /// Hashes of the items already stored, oldest first.
    #[serde(default, skip_serializing)]
    pub seen: Vec<String>,
}

fn yes() -> bool {
    true
}

/// `signals.feed_add`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FeedInput {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub sensitivity: Option<Sensitivity>,
    /// Keep items as knowledge documents too (default true).
    #[serde(default)]
    pub knowledge: Option<bool>,
    /// Items as events only (`knowledge = false`; what gRPC sends).
    #[serde(default)]
    pub events_only: bool,
}

/// One item of a feed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedItem {
    pub guid: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub link: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Milliseconds since the epoch, when the feed says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FetchReport {
    pub feed: String,
    /// Items in the feed document.
    pub items: usize,
    /// Items stored this time (the others were stored before).
    pub stored: usize,
    /// The knowledge documents written.
    pub documents: usize,
}

/// `signals.recommend` and `signals.propose`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RecommendInput {
    #[serde(default)]
    pub workspace: Option<String>,
    /// Only these feeds' items (default: every feed's).
    #[serde(default)]
    pub feeds: Vec<String>,
    /// Only items stored since (ms).
    #[serde(default)]
    pub since: Option<i64>,
    /// Items that must name a pair before it suggests anything (default 3).
    #[serde(default)]
    pub min_support: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// An item that backs a suggestion.
#[derive(Clone, Debug, Serialize)]
pub struct Evidence {
    pub event: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub link: String,
    /// The entities it names that the suggestion is about.
    pub entities: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Suggestion {
    /// `link_type` or `rule`.
    pub kind: String,
    /// The proposal's title: one suggestion is proposed once while open.
    pub title: String,
    pub rationale: String,
    /// Items that name the pairs it is about.
    pub support: usize,
    pub evidence: Vec<Evidence>,
    pub change_set: SchemaChangeSet,
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok { Ok(()) } else { Err(Error::invalid("feed name: [a-z0-9_-], 1 to 40 characters")) }
}

fn hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

fn admin_only(p: &Principal, what: &str) -> Result<()> {
    if p.is_agent() || !p.is_privileged() {
        return Err(Error::Denied(format!("{} may not {what} (admins can)", p.id)));
    }
    Ok(())
}

// ── parsing ────────────────────────────────────────────────────────────

/// The items of an RSS 2.0, RSS 1.0 (RDF) or Atom document. A guid is the
/// item's `guid` or `id`, else its link, else its title; items with none
/// of them are skipped.
pub fn parse_feed(xml: &str) -> Result<Vec<FeedItem>> {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    // Text is not trimmed while read: `A &amp; B` arrives as text, a
    // reference and text, and trimming the parts would lose the spaces.
    let mut reader = Reader::from_str(xml);
    let mut items = Vec::new();
    let mut item: Option<FeedItem> = None;
    // The local name of the element whose text is being read inside an item.
    let mut field: Option<String> = None;
    let mut text = String::new();
    let mut saw_feed = false;
    let local = |name: &str| -> String { name.rsplit(':').next().unwrap_or_default().to_ascii_lowercase() };
    loop {
        match reader.read_event() {
            Err(e) => return Err(Error::invalid(format!("feed: not well-formed XML: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = local(e.name().as_ref());
                match name.as_str() {
                    "rss" | "rdf" | "feed" => saw_feed = true,
                    "item" | "entry" => {
                        item = Some(FeedItem::default());
                        field = None;
                    }
                    "link" if item.is_some() => {
                        // Atom: <link href="…" rel="alternate"/> (also as a start tag).
                        if let Some(href) = atom_link(&e) {
                            let it = item.as_mut().expect("in an item");
                            if it.link.is_empty() {
                                it.link = href;
                            }
                        }
                        field = Some(name);
                        text.clear();
                    }
                    _ if item.is_some() => {
                        field = Some(name);
                        text.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                if local(e.name().as_ref()) == "link"
                    && let (Some(it), Some(href)) = (item.as_mut(), atom_link(&e))
                    && it.link.is_empty()
                {
                    it.link = href;
                }
            }
            Ok(Event::Text(t)) => {
                if field.is_some() {
                    let s: &str = &t;
                    text.push_str(&quick_xml::escape::unescape(s).unwrap_or(s.into()));
                }
            }
            Ok(Event::CData(t)) => {
                if field.is_some() {
                    text.push_str(&t);
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if field.is_some() {
                    text.push_str(&entity(&r));
                }
            }
            Ok(Event::End(e)) => {
                let name = local(e.name().as_ref());
                if name == "item" || name == "entry" {
                    if let Some(mut it) = item.take() {
                        if it.guid.is_empty() {
                            it.guid = if it.link.is_empty() { it.title.clone() } else { it.link.clone() };
                        }
                        if !it.guid.is_empty() {
                            it.summary = squeeze(&strip_html(&it.summary));
                            it.title = squeeze(&strip_html(&it.title));
                            items.push(it);
                        }
                        if items.len() >= MAX_ITEMS {
                            break;
                        }
                    }
                    field = None;
                } else if let (Some(it), Some(f)) = (item.as_mut(), field.as_deref())
                    && f == name
                {
                    let value = text.trim().to_string();
                    match f {
                        "title" => it.title = value,
                        "link" if it.link.is_empty() => it.link = value,
                        "guid" | "id" => it.guid = value,
                        "description" | "summary" if it.summary.is_empty() => it.summary = value,
                        // content:encoded and Atom content win over a short description.
                        "encoded" | "content" if value.len() > it.summary.len() => it.summary = value,
                        "pubdate" | "published" | "updated" | "date" if it.published.is_none() => {
                            it.published = parse_date(&value);
                        }
                        _ => {}
                    }
                    field = None;
                    text.clear();
                }
            }
            Ok(_) => {}
        }
    }
    if !saw_feed {
        return Err(Error::invalid("feed: neither RSS nor Atom (no <rss>, <rdf:RDF> or <feed>)"));
    }
    Ok(items)
}

/// An Atom link's `href` when it is the alternate (or unqualified) one.
fn atom_link(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    let mut href = None;
    let mut rel = None;
    for a in e.attributes().flatten() {
        let v = a.normalized_value(quick_xml::XmlVersion::default()).ok()?.into_owned();
        match a.key.as_ref() {
            "href" => href = Some(v),
            "rel" => rel = Some(v),
            _ => {}
        }
    }
    href.filter(|_| rel.as_deref().is_none_or(|r| r == "alternate"))
}

/// The text of an XML or HTML character reference or named entity.
fn entity(name: &str) -> String {
    let named = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => None,
    };
    if let Some(c) = named {
        return c.to_string();
    }
    let code = name
        .strip_prefix("#x")
        .or_else(|| name.strip_prefix("#X"))
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()));
    code.and_then(char::from_u32).map(String::from).unwrap_or_default()
}

/// Text of an HTML fragment: tags dropped, common entities decoded.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let mut decoded = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find('&') {
        decoded.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find(';').filter(|j| *j <= 10) {
            Some(j) if !entity(&after[..j]).is_empty() => {
                decoded.push_str(&entity(&after[..j]));
                rest = &after[j + 1..];
            }
            _ => {
                decoded.push('&');
                rest = after;
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// RFC 3339 (Atom, `dc:date`) or RFC 822/2822 (RSS `pubDate`) to ms.
pub fn parse_date(s: &str) -> Option<i64> {
    onto_core::time::parse_ts(s).or_else(|| rfc2822(s))
}

fn rfc2822(s: &str) -> Option<i64> {
    // [Day,] DD Mon YYYY HH:MM[:SS] (+hhmm | -hhmm | GMT | UT | Z | EST …)
    let s = s.trim();
    let s = s.split_once(',').map_or(s, |(_, rest)| rest).trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let day: u32 = parts[0].parse().ok()?;
    let month = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|m| parts[1].to_ascii_lowercase().starts_with(m))? as u32
        + 1;
    let mut year: i64 = parts[2].parse().ok()?;
    if year < 100 {
        year += if year < 50 { 2000 } else { 1900 };
    }
    let hms: Vec<u32> = parts[3].split(':').map(|x| x.parse().ok()).collect::<Option<Vec<_>>>()?;
    let (h, mi, se) = (*hms.first()?, *hms.get(1)?, hms.get(2).copied().unwrap_or(0));
    let offset_min: i64 = match parts.get(4).map(|z| z.to_ascii_uppercase()) {
        None => 0,
        Some(z) if z == "GMT" || z == "UT" || z == "UTC" || z == "Z" => 0,
        Some(z) if z == "EST" => -300,
        Some(z) if z == "EDT" => -240,
        Some(z) if z == "CST" => -360,
        Some(z) if z == "CDT" => -300,
        Some(z) if z == "MST" => -420,
        Some(z) if z == "MDT" => -360,
        Some(z) if z == "PST" => -480,
        Some(z) if z == "PDT" => -420,
        Some(z) if (z.starts_with('+') || z.starts_with('-')) && z.len() == 5 => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let hh: i64 = z[1..3].parse().ok()?;
            let mm: i64 = z[3..5].parse().ok()?;
            sign * (hh * 60 + mm)
        }
        Some(_) => 0,
    };
    let iso = format!("{year:04}-{month:02}-{day:02}T{h:02}:{mi:02}:{se:02}Z");
    Some(onto_core::time::parse_ts(&iso)? - offset_min * 60_000)
}

// ── fetching ───────────────────────────────────────────────────────────

/// The reach of a feed: public addresses only, no operator allow rules;
/// `allow_private` (`[signals] allow_private_urls`, development) also
/// reaches loopback and private ranges, never link-local or metadata ones.
pub fn reach(allow_private: bool) -> crate::net::Reach {
    crate::net::Reach { restricted: true, allow_private, ..Default::default() }
}

/// The feed document at `url`, following at most three redirects, each
/// checked as the first URL is.
#[cfg(feature = "outbound")]
pub async fn fetch_url(url: &str, reach: &crate::net::Reach) -> Result<String> {
    use reqwest::header::{ACCEPT, LOCATION, USER_AGENT};
    let mut url = crate::net::check_target_url(url, "feed url", reach)?;
    let client = crate::net::guarded_client(reach, std::time::Duration::from_secs(15))?;
    for _ in 0..=3 {
        let mut resp = client
            .get(url.clone())
            .header(ACCEPT, "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.8")
            .header(USER_AGENT, concat!("onto-intelli-signals/", env!("CARGO_PKG_VERSION")))
            .send()
            .await
            .map_err(|e| {
                Error::Unavailable(format!("feed {}: {}", crate::webhooks::url_origin(url.as_str()), e.without_url()))
            })?;
        if resp.status().is_redirection() {
            let to = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Unavailable("feed: a redirect without Location".into()))?;
            let next = url.join(to).map_err(|e| Error::Unavailable(format!("feed: redirect to {to}: {e}")))?;
            url = crate::net::check_target_url(next.as_str(), "feed redirect", reach)?;
            continue;
        }
        if !resp.status().is_success() {
            return Err(Error::Unavailable(format!("feed answered {}", resp.status())));
        }
        let mut body = Vec::new();
        while let Some(chunk) =
            resp.chunk().await.map_err(|e| Error::Unavailable(format!("feed: {}", e.without_url())))?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_FEED_BYTES {
                return Err(Error::Unavailable(format!("feed: larger than {MAX_FEED_BYTES} bytes")));
            }
        }
        return Ok(String::from_utf8_lossy(&body).into_owned());
    }
    Err(Error::Unavailable("feed: more than three redirects".into()))
}

// ── feeds and items ────────────────────────────────────────────────────

impl Intelli {
    fn feed(&self, name: &str) -> Result<Feed> {
        let v =
            self.ontology().store().kv_get(NS_FEED, name)?.ok_or_else(|| Error::not_found(format!("feed {name}")))?;
        serde_json::from_value(v).map_err(|e| Error::Storage(format!("feed {name}: {e}")))
    }

    fn save_feed(&self, f: &Feed) -> Result<()> {
        let mut v = serde_json::to_value(f)?;
        // `seen` is not shown, but it is kept.
        v["seen"] = json!(f.seen);
        self.ontology().store().kv_put(NS_FEED, &f.name, &v, None)
    }

    /// Register a feed (admins). Its URL is checked now and on every fetch.
    pub fn feed_add(&self, p: &Principal, input: FeedInput) -> Result<Feed> {
        admin_only(p, "register feeds")?;
        check_name(&input.name)?;
        let reach = reach(self.config().signals.allow_private_urls);
        let url = crate::net::check_target_url(&input.url, "feed url", &reach)?;
        let ws = input.workspace.filter(|w| !w.is_empty()).unwrap_or_else(|| self.default_workspace().to_string());
        if !p.in_workspace(&ws) {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        if self.ontology().store().kv_get(NS_FEED, &input.name)?.is_some() {
            return Err(Error::Conflict(format!("feed {} exists", input.name)));
        }
        let feed = Feed {
            collection: input
                .collection
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| format!("signals:{}", input.name)),
            name: input.name,
            url: url.to_string(),
            workspace: ws,
            sensitivity: input.sensitivity.unwrap_or(Sensitivity::Public),
            knowledge: input.knowledge.unwrap_or(!input.events_only),
            created_by: p.id.clone(),
            created_at: now_ms(),
            last_fetch: None,
            last_error: None,
            items: 0,
            seen: Vec::new(),
        };
        self.save_feed(&feed)?;
        let ctx = ActionContext::new(p.clone(), feed.workspace.clone()).with_intent("signals.feed_add");
        let audit = EventInput {
            action: "signals.feed_add".into(),
            payload: json!({ "feed": feed.name, "origin": crate::webhooks::url_origin(&feed.url) }),
            ..Default::default()
        };
        self.ontology().record_event(&ctx, audit)?;
        Ok(feed)
    }

    pub fn feeds(&self, p: &Principal) -> Result<Vec<Feed>> {
        if !p.allows(onto_core::Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let mut out: Vec<Feed> = self
            .ontology()
            .store()
            .kv_scan(NS_FEED, "")?
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_value::<Feed>(v).ok())
            .filter(|f| p.in_workspace(&f.workspace))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Remove a feed (admins). Its items stay: they are events and documents.
    pub fn feed_remove(&self, p: &Principal, name: &str) -> Result<bool> {
        admin_only(p, "remove feeds")?;
        let Ok(feed) = self.feed(name) else { return Ok(false) };
        if !p.in_workspace(&feed.workspace) {
            return Ok(false);
        }
        self.ontology().store().kv_delete(NS_FEED, name)?;
        Ok(true)
    }

    /// Fetch a feed now (admins) and store its new items.
    #[cfg(feature = "outbound")]
    pub async fn feed_fetch(&self, p: &Principal, name: &str) -> Result<FetchReport> {
        admin_only(p, "fetch feeds")?;
        let feed = self.feed(name)?;
        if !p.in_workspace(&feed.workspace) {
            return Err(Error::not_found(format!("feed {name}")));
        }
        let reach = reach(self.config().signals.allow_private_urls);
        let fetched = match fetch_url(&feed.url, &reach).await {
            Ok(xml) => parse_feed(&xml),
            Err(e) => Err(e),
        };
        let mut feed = feed;
        feed.last_fetch = Some(now_ms());
        let items = match fetched {
            Ok(items) => {
                feed.last_error = None;
                items
            }
            Err(e) => {
                feed.last_error = Some(clip(&e.to_string(), 300));
                self.save_feed(&feed)?;
                return Err(e);
            }
        };
        let report = self.store_items(p, &mut feed, &items)?;
        self.save_feed(&feed)?;
        Ok(report)
    }

    /// Store the items `feed` has not stored yet: one event each in one
    /// commit, and a knowledge document each when the feed keeps them.
    pub fn store_items(&self, p: &Principal, feed: &mut Feed, items: &[FeedItem]) -> Result<FetchReport> {
        let seen: HashSet<&str> = feed.seen.iter().map(String::as_str).collect();
        let mut fresh: Vec<(&FeedItem, String)> = Vec::new();
        for it in items {
            let h = hash(&[&feed.name, &it.guid]);
            if !seen.contains(h.as_str()) && !fresh.iter().any(|(_, x)| *x == h) {
                fresh.push((it, h));
            }
        }
        let mut report = FetchReport { feed: feed.name.clone(), items: items.len(), ..Default::default() };
        if fresh.is_empty() {
            return Ok(report);
        }
        let ctx = ActionContext::new(p.clone(), feed.workspace.clone()).with_intent("signals.fetch");
        let source = format!("feed:{}", feed.name);
        let muts: Vec<Mutation> = fresh
            .iter()
            .map(|(it, h)| {
                Mutation::Event(EventInput {
                    action: ITEM_ACTION.into(),
                    workspace: Some(feed.workspace.clone()),
                    source: Some(source.clone()),
                    payload: json!({
                        "feed": feed.name,
                        "item": h,
                        "guid": clip(&it.guid, 500),
                        "title": clip(&it.title, 500),
                        "link": clip(&it.link, 2000),
                        "summary": clip(&it.summary, EVENT_SUMMARY_CHARS),
                        "published": it.published,
                    }),
                    ..Default::default()
                })
            })
            .collect();
        self.ontology().apply(&ctx, muts)?;
        report.stored = fresh.len();
        if feed.knowledge {
            for (it, h) in &fresh {
                let text =
                    if it.summary.is_empty() { it.title.clone() } else { format!("{}\n\n{}", it.title, it.summary) };
                if text.trim().is_empty() {
                    continue;
                }
                let input = IngestInput {
                    id: Some(format!("sig-{}-{h}", feed.name)),
                    name: clip(if it.title.is_empty() { &it.link } else { &it.title }, 250),
                    workspace: Some(feed.workspace.clone()),
                    collection: Some(feed.collection.clone()),
                    sensitivity: Some(feed.sensitivity),
                    source: Some(it.link.clone()).filter(|l| !l.is_empty()),
                    meta: json!({ "feed": feed.name, "guid": it.guid, "published": it.published }),
                    text: Some(text),
                    ..Default::default()
                };
                self.knowledge().ingest(p, &feed.workspace, input)?;
                report.documents += 1;
            }
        }
        feed.items += fresh.len() as u64;
        feed.seen.extend(fresh.into_iter().map(|(_, h)| h));
        let over = feed.seen.len().saturating_sub(SEEN_MAX);
        feed.seen.drain(..over);
        Ok(report)
    }

    /// The `signal.item` events `p` may read.
    fn signal_items(&self, p: &Principal, input: &RecommendInput) -> Result<Vec<onto_core::Event>> {
        let filter = EventFilter {
            action: Some(ITEM_ACTION.into()),
            workspace: input.workspace.clone(),
            since: input.since,
            limit: Some(5_000),
            ..Default::default()
        };
        let feeds: HashSet<&str> = input.feeds.iter().map(String::as_str).collect();
        Ok(self
            .ontology()
            .events(p, &filter)?
            .into_iter()
            .filter(|e| feeds.is_empty() || e.payload["feed"].as_str().is_some_and(|f| feeds.contains(f)))
            .collect())
    }

    // ── recommendations ────────────────────────────────────────────────

    /// What the feeds' items suggest about the schema. Reads only.
    pub fn recommend(&self, p: &Principal, input: &RecommendInput) -> Result<Vec<Suggestion>> {
        let min_support = input.min_support.unwrap_or(3).max(2);
        let limit = input.limit.unwrap_or(10).clamp(1, 50);
        let items = self.signal_items(p, input)?;
        let onto = self.ontology();
        let spec = FindSpec { workspace: input.workspace.clone(), limit: 200_000, ..Default::default() };
        let (entities, _) = onto.find(p, &spec)?;
        let names = NameIndex::new(&entities);
        // The entities each item names.
        let mut named: Vec<(&onto_core::Event, Vec<&Entity>)> = Vec::new();
        for e in &items {
            let text = format!(
                "{} {}",
                e.payload["title"].as_str().unwrap_or_default(),
                e.payload["summary"].as_str().unwrap_or_default()
            );
            let found = names.find(&text);
            if found.len() >= 2 {
                named.push((e, found));
            }
        }
        let schema = onto.schema();
        // Unlinked pairs named together, by the kinds of their two ends.
        let mut by_kinds: BTreeMap<(String, String), PairStats> = BTreeMap::new();
        let mut linked: HashMap<(String, String), bool> = HashMap::new();
        for (ev, found) in &named {
            for (i, a) in found.iter().enumerate() {
                for b in &found[i + 1..] {
                    let (a, b) = if (&a.kind, &a.id) <= (&b.kind, &b.id) { (*a, *b) } else { (*b, *a) };
                    let key = (a.id.clone(), b.id.clone());
                    let is_linked = *linked.entry(key.clone()).or_insert_with(|| {
                        onto.neighbors(p, &a.id, None).is_ok_and(|n| n.iter().any(|(_, e)| e.id == b.id))
                    });
                    if is_linked {
                        continue;
                    }
                    let s = by_kinds.entry((a.kind.clone(), b.kind.clone())).or_default();
                    s.items.insert(ev.id.clone());
                    s.pairs.insert(key);
                    if s.evidence.len() < 5 && !s.evidence.iter().any(|x: &Evidence| x.event == ev.id) {
                        s.evidence.push(evidence(ev, &[&a.id, &b.id]));
                    }
                }
            }
        }
        let mut out = Vec::new();
        let feeds = feeds_of(&items);
        for ((ka, kb), s) in &by_kinds {
            if s.items.len() < min_support || has_link_type(&schema, ka, kb) {
                continue;
            }
            let name = link_name(ka, kb);
            if schema.link_types.contains_key(&name) {
                continue;
            }
            let lt = LinkType {
                name: name.clone(),
                label: "named together with".into(),
                description: format!(
                    "Proposed from external signals: {} {ka} and {kb} pairs are named together in {} items of {feeds} \
                     and are not linked.",
                    s.pairs.len(),
                    s.items.len()
                ),
                from: vec![ka.clone()],
                to: vec![kb.clone()],
                symmetric: ka == kb,
                ..empty_link_type()
            };
            out.push(Suggestion {
                kind: "link_type".into(),
                title: format!("Link type {name} ({ka} and {kb} named together in the news)"),
                rationale: format!(
                    "{} items of {feeds} name a {ka} and a {kb} together ({} pairs), and the schema has no link type \
                     between {ka} and {kb}. Review the evidence, rename the link type if a sharper relation fits, \
                     then approve or reject.",
                    s.items.len(),
                    s.pairs.len()
                ),
                support: s.items.len(),
                evidence: s.evidence.clone(),
                change_set: SchemaChangeSet {
                    changes: vec![SchemaChange::PutLinkType { name, def: lt, migrations: Vec::new() }],
                    message: "Proposed by signals.recommend".into(),
                    ..Default::default()
                },
            });
            // A two-step path joining the same pairs suggests a chain rule.
            if let Some(rule) = self.chain_rule(p, &schema, ka, kb, s, min_support, &feeds) {
                out.push(rule);
            }
        }
        out.sort_by(|a, b| b.support.cmp(&a.support).then_with(|| a.title.cmp(&b.title)));
        out.truncate(limit);
        Ok(out)
    }

    /// The chain rule (`r1`, `r2` ⇒ the suggested link type) that the most
    /// pairs of `s` are joined by, when at least `min_support` are.
    #[allow(clippy::too_many_arguments)]
    fn chain_rule(
        &self,
        p: &Principal,
        schema: &onto_core::Schema,
        ka: &str,
        kb: &str,
        s: &PairStats,
        min_support: usize,
        feeds: &str,
    ) -> Option<Suggestion> {
        let onto = self.ontology();
        let mut paths: BTreeMap<(String, String), BTreeSet<(String, String)>> = BTreeMap::new();
        for (a, b) in s.pairs.iter().take(500) {
            let Ok(first) = onto.neighbors(p, a, None) else { continue };
            for (r1, mid) in first.iter().take(50) {
                let Ok(second) = onto.neighbors(p, &mid.id, None) else { continue };
                for (r2, end) in second.iter().take(50) {
                    if &end.id == b {
                        paths.entry((r1.clone(), r2.clone())).or_default().insert((a.clone(), b.clone()));
                    }
                }
            }
        }
        let ((r1, r2), joined) = paths.into_iter().max_by_key(|(_, pairs)| pairs.len())?;
        if joined.len() < min_support || schema.rules.iter().any(|r| r.chain == [r1.clone(), r2.clone()]) {
            return None;
        }
        let infer = link_name(ka, kb);
        let name = format!("signals_{}_{}", snake(&r1), snake(&r2));
        let rule = Rule {
            name: name.clone(),
            description: format!("Proposed from external signals: {ka} -{r1}-> · -{r2}-> {kb} implies {infer}."),
            chain: vec![r1.clone(), r2.clone()],
            infer: infer.clone(),
            ..Default::default()
        };
        Some(Suggestion {
            kind: "rule".into(),
            title: format!("Rule {name} ({r1} then {r2} implies {infer})"),
            rationale: format!(
                "Of the {ka}/{kb} pairs named together in {feeds}, {} are joined by {r1} then {r2} in the graph. \
                 The rule infers {infer} for every such path; it needs the link type {infer} (proposed alongside).",
                joined.len()
            ),
            support: joined.len(),
            evidence: s
                .evidence
                .iter()
                .filter(|e| joined.iter().any(|(a, b)| e.entities.contains(a) && e.entities.contains(b)))
                .cloned()
                .collect(),
            change_set: SchemaChangeSet {
                changes: vec![SchemaChange::PutRule { def: rule }],
                message: "Proposed by signals.recommend".into(),
                ..Default::default()
            },
        })
    }

    /// File each suggestion as a schema proposal (source `signals`), unless
    /// an open proposal has its title. Proposals are reviewed and applied by
    /// people; nothing is applied here.
    pub fn propose_signals(&self, p: &Principal, input: &RecommendInput) -> Result<Value> {
        let suggestions = self.recommend(p, input)?;
        let onto = self.ontology();
        let open: HashSet<String> =
            onto.list_proposals(p, Some(ProposalStatus::Open), 1_000)?.into_iter().map(|v| v.proposal.title).collect();
        let ws = input.workspace.clone().unwrap_or_else(|| self.default_workspace().to_string());
        let ctx = ActionContext::new(p.clone(), ws).with_intent("signals.propose");
        let mut filed = Vec::new();
        let mut skipped = Vec::new();
        // A rule needs its link type: propose them together.
        let mut by_infer: HashMap<String, Suggestion> = HashMap::new();
        let mut queue: Vec<Suggestion> = Vec::new();
        for s in suggestions {
            if s.kind == "rule" {
                let infer = match &s.change_set.changes[0] {
                    SchemaChange::PutRule { def } => def.infer.clone(),
                    _ => String::new(),
                };
                by_infer.insert(infer, s);
            } else {
                queue.push(s);
            }
        }
        for mut s in queue {
            if let Some(SchemaChange::PutLinkType { name, .. }) = s.change_set.changes.first()
                && let Some(rule) = by_infer.remove(name)
            {
                s.change_set.changes.extend(rule.change_set.changes);
                s.rationale = format!("{}\n\n{}", s.rationale, rule.rationale);
                s.title = format!("{} with rule {}", s.title, rule.title);
            }
            if open.contains(&s.title) {
                skipped.push(json!({ "title": s.title, "why": "an open proposal has this title" }));
                continue;
            }
            let mut rationale = s.rationale.clone();
            for e in &s.evidence {
                rationale.push_str(&format!("\n- {} {}", e.title, e.link));
            }
            let proposal = ProposalInput {
                title: s.title.clone(),
                rationale,
                change_set: s.change_set.clone(),
                source: Some("signals".into()),
            };
            match onto.propose_schema(&ctx, proposal) {
                Ok(v) => filed.push(json!({ "id": v.proposal.id, "title": s.title, "support": s.support })),
                Err(e) => skipped.push(json!({ "title": s.title, "why": e.to_string() })),
            }
        }
        Ok(json!({ "proposed": filed, "skipped": skipped }))
    }
}

#[derive(Default)]
struct PairStats {
    items: BTreeSet<String>,
    pairs: BTreeSet<(String, String)>,
    evidence: Vec<Evidence>,
}

fn evidence(ev: &onto_core::Event, entities: &[&str]) -> Evidence {
    Evidence {
        event: ev.id.clone(),
        title: ev.payload["title"].as_str().unwrap_or_default().to_string(),
        link: ev.payload["link"].as_str().unwrap_or_default().to_string(),
        entities: entities.iter().map(|s| s.to_string()).collect(),
    }
}

fn feeds_of(items: &[onto_core::Event]) -> String {
    let names: BTreeSet<&str> = items.iter().filter_map(|e| e.payload["feed"].as_str()).collect();
    match names.len() {
        0 => "no feed".into(),
        1 => format!("feed {}", names.iter().next().expect("one")),
        _ => format!("feeds {}", names.into_iter().collect::<Vec<_>>().join(", ")),
    }
}

/// A link type between `a` and `b` (either way) that names both kinds.
fn has_link_type(schema: &onto_core::Schema, a: &str, b: &str) -> bool {
    let names = |kinds: &[String], k: &str| kinds.iter().any(|x| x == k || schema.kind_is(k, x));
    schema
        .link_types
        .values()
        .any(|lt| (names(&lt.from, a) && names(&lt.to, b)) || (names(&lt.from, b) && names(&lt.to, a)))
}

fn snake(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
}

fn link_name(a: &str, b: &str) -> String {
    format!("{}_named_with_{}", snake(a), snake(b))
}

fn empty_link_type() -> LinkType {
    serde_json::from_value(json!({})).expect("a link type with defaults")
}

/// Entity titles by their words, to find the entities a text names.
struct NameIndex<'a> {
    /// Normalized title (lowercase words joined by a space) → entities.
    by_title: HashMap<String, Vec<&'a Entity>>,
    /// The most words a title has.
    longest: usize,
}

/// Lowercase alphanumeric words.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

impl<'a> NameIndex<'a> {
    fn new(entities: &'a [Entity]) -> Self {
        let mut by_title: HashMap<String, Vec<&'a Entity>> = HashMap::new();
        let mut longest = 1;
        for e in entities {
            for name in std::iter::once(&e.title).chain(&e.aliases) {
                let w = words(name);
                // One short word ("API", "Q3") names too much to count.
                let chars: usize = w.iter().map(|x| x.chars().count()).sum();
                if w.is_empty() || w.len() > 6 || (w.len() == 1 && chars < 4) {
                    continue;
                }
                longest = longest.max(w.len());
                let list = by_title.entry(w.join(" ")).or_default();
                if !list.iter().any(|x| x.id == e.id) {
                    list.push(e);
                }
            }
        }
        Self { by_title, longest }
    }

    /// The entities `text` names by their whole title, each once.
    fn find(&self, text: &str) -> Vec<&'a Entity> {
        let w = words(text);
        let mut out: Vec<&'a Entity> = Vec::new();
        for i in 0..w.len() {
            for n in 1..=self.longest.min(w.len() - i) {
                if let Some(es) = self.by_title.get(&w[i..i + n].join(" ")) {
                    for e in es {
                        if !out.iter().any(|x| x.id == e.id) {
                            out.push(e);
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
<channel><title>Wire</title>
<item><title>Acme &amp; Payment API outage</title><link>https://news.example.com/1</link>
<guid isPermaLink="false">wire-1</guid><pubDate>Tue, 10 Jun 2025 04:00:00 +0200</pubDate>
<description><![CDATA[<p>The <b>Payment API</b> failed for an hour.</p>]]></description></item>
<item><title>No guid here</title><link>https://news.example.com/2</link></item>
<item><description>nothing to name it by</description></item>
</channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>Blog</title>
<entry><title type="html">Kenji &lt;b&gt;joins&lt;/b&gt;</title>
<link rel="self" href="https://blog.example.com/self"/><link href="https://blog.example.com/kenji"/>
<id>urn:uuid:1</id><updated>2025-06-11T08:30:00Z</updated>
<summary>Kenji Tanaka moves to payments.</summary><content type="html">&lt;p&gt;Kenji Tanaka moves to the payments team, effective today.&lt;/p&gt;</content></entry>
</feed>"#;

    #[test]
    fn rss_and_atom_items_are_read() {
        let items = parse_feed(RSS).unwrap();
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].title, "Acme & Payment API outage");
        assert_eq!(items[0].guid, "wire-1");
        assert_eq!(items[0].summary, "The Payment API failed for an hour.");
        assert_eq!(items[0].published, onto_core::time::parse_ts("2025-06-10T02:00:00Z"));
        assert_eq!(items[1].guid, "https://news.example.com/2", "the link stands in for a missing guid");
        let atom = parse_feed(ATOM).unwrap();
        assert_eq!(atom[0].link, "https://blog.example.com/kenji", "the alternate link, not self");
        assert_eq!(atom[0].title, "Kenji joins");
        assert_eq!(atom[0].summary, "Kenji Tanaka moves to the payments team, effective today.");
        assert_eq!(atom[0].published, onto_core::time::parse_ts("2025-06-11T08:30:00Z"));
        assert!(parse_feed("<html><body>not a feed</body></html>").is_err());
        assert!(parse_feed("<rss><channel><item><title>x</item></rss>").is_err(), "not well-formed");
    }

    /// `fetch_url` has already turned the bytes into text, so the parser
    /// neither follows a declared encoding nor refuses it, and a BOM (kept
    /// by the lossy decode) is no reason to reject the document.
    #[test]
    fn a_declared_encoding_or_a_bom_changes_nothing() {
        let body = r#"<rss version="2.0"><channel><item><guid>1</guid><title>Caf&#233; &amp; bar</title></item></channel></rss>"#;
        for head in [r#"<?xml version="1.0" encoding="ISO-8859-1"?>"#, "\u{feff}<?xml version=\"1.0\"?>", "\u{feff}"] {
            let items = parse_feed(&format!("{head}{body}")).unwrap_or_else(|e| panic!("{head}: {e}"));
            assert_eq!(items[0].title, "Café & bar", "{head}");
        }
    }

    #[test]
    fn dates_in_both_formats() {
        assert_eq!(rfc2822("Mon, 02 Jan 2006 15:04:05 GMT"), onto_core::time::parse_ts("2006-01-02T15:04:05Z"));
        assert_eq!(rfc2822("2 Jan 06 15:04 -0700"), onto_core::time::parse_ts("2006-01-02T22:04:00Z"));
        assert_eq!(rfc2822("Mon, 02 Jan 2006 10:04:05 EST"), onto_core::time::parse_ts("2006-01-02T15:04:05Z"));
        assert_eq!(rfc2822("yesterday"), None);
    }

    #[test]
    fn titles_are_found_as_whole_words() {
        let e = |id: &str, title: &str, aliases: &[&str]| Entity {
            id: id.into(),
            kind: id.split(':').next().unwrap_or_default().into(),
            workspace: "acme".into(),
            title: title.into(),
            state: None,
            props: Default::default(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            provenance: None,
            created_at: 0,
            updated_at: 0,
            revision: 1,
        };
        let es = [
            e("service:pay", "Payment API", &["PayAPI"]),
            e("person:kenji", "Kenji Tanaka", &[]),
            e("x:api", "API", &[]),
            e("y:pay", "Pay", &[]),
        ];
        let idx = NameIndex::new(&es);
        let ids: Vec<&str> = idx
            .find("Kenji Tanaka says the payment api is back; Payments API is not")
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, ["person:kenji", "service:pay"]);
        assert_eq!(idx.find("payapi is down").len(), 1, "aliases name an entity too");
    }

    #[test]
    fn feed_names_and_urls_are_checked() {
        assert!(check_name("wire_1").is_ok());
        for bad in ["", "Wire", "a/b", &"x".repeat(41)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        let public = reach(false);
        assert!(crate::net::check_target_url("https://news.example.com/rss", "feed url", &public).is_ok());
        for bad in [
            "http://news.example.com/rss",
            "https://127.0.0.1/rss",
            "https://169.254.169.254/latest",
            "file:///etc/passwd",
            "https://u:p@news.example.com/",
        ] {
            assert!(crate::net::check_target_url(bad, "feed url", &public).is_err(), "{bad}");
        }
        // Development: loopback, never metadata.
        let dev = reach(true);
        assert!(crate::net::check_target_url("http://127.0.0.1:8080/rss", "feed url", &dev).is_ok());
        assert!(crate::net::check_target_url("http://169.254.169.254/latest", "feed url", &dev).is_err());
    }
}
