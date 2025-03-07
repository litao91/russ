//! The functions and datatypes in this module all for the retrieval and storage
//! of RSS/Atom feeds in Russ' SQLite database.

use crate::modes::ReadMode;
use anyhow::{bail, Context, Result};
use atom_syndication as atom;
use chrono::prelude::{DateTime, Utc};
use html_escape::decode_html_entities_to_string;
use rss::Channel;
use rusqlite::params;
use rusqlite::types::{FromSql, ToSqlOutput};
use std::collections::HashSet;
use std::fmt::Display;
use std::str::FromStr;
use ureq::http;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EntryId(i64);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FeedId(i64);

impl From<i64> for EntryId {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl rusqlite::ToSql for EntryId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.0.into())
    }
}

impl FromSql for EntryId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(value.as_i64()?))
    }
}

impl Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<i64> for FeedId {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl rusqlite::ToSql for FeedId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.0.into())
    }
}

impl FromSql for FeedId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(value.as_i64()?))
    }
}

impl Display for FeedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum FeedKind {
    Atom,
    Rss,
}

impl rusqlite::types::FromSql for FeedKind {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = value.as_str()?;
        match FeedKind::from_str(s) {
            Ok(feed_kind) => Ok(feed_kind),
            Err(e) => Err(rusqlite::types::FromSqlError::Other(e.into())),
        }
    }
}

impl rusqlite::types::ToSql for FeedKind {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let s = self.to_string();
        Ok(ToSqlOutput::from(s))
    }
}

impl Display for FeedKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let out = match self {
            FeedKind::Atom => "Atom",
            FeedKind::Rss => "RSS",
        };

        write!(f, "{out}")
    }
}

impl FromStr for FeedKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Atom" => Ok(FeedKind::Atom),
            "RSS" => Ok(FeedKind::Rss),
            _ => Err(anyhow::anyhow!(format!("{s} is not a valid FeedKind"))),
        }
    }
}

/// Feed metadata.
/// Entries are stored separately.
/// The `id` of this type corresponds to `feed_id` on
/// `Entry` and `EntryMeta`.
#[derive(Clone, Debug)]
pub struct Feed {
    pub id: FeedId,
    pub title: Option<String>,
    pub feed_link: Option<String>,
    pub link: Option<String>,
    pub feed_kind: FeedKind,
    pub refreshed_at: Option<chrono::DateTime<Utc>>,
    pub inserted_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
    pub latest_etag: Option<String>,
}

/// This exists:
/// 1. So we can validate an incoming Atom/RSS feed
/// 2. So we can insert it into the database
struct IncomingFeed {
    title: Option<String>,
    feed_link: Option<String>,
    link: Option<String>,
    feed_kind: FeedKind,
    latest_etag: Option<String>,
}

/// This exists:
/// 1. So we can validate an incoming Atom/RSS feed entry
/// 2. So we can insert it into the database
struct IncomingEntry {
    title: Option<String>,
    author: Option<String>,
    pub_date: Option<chrono::DateTime<Utc>>,
    description: Option<String>,
    content: Option<String>,
    link: Option<String>,
}

impl From<&atom::Entry> for IncomingEntry {
    fn from(entry: &atom::Entry) -> Self {
        Self {
            title: {
                let mut title = String::new();
                decode_html_entities_to_string(entry.title(), &mut title);
                Some(title)
            },
            author: entry.authors().first().map(|entry_author| {
                let mut author = String::new();
                decode_html_entities_to_string(&entry_author.name, &mut author);
                author
            }),
            pub_date: entry.published().map(|date| date.with_timezone(&Utc)),
            description: None,
            content: entry.content().and_then(|entry_content| {
                entry_content.value().map(|entry_content| {
                    let mut content = String::new();
                    decode_html_entities_to_string(entry_content, &mut content);
                    content
                })
            }),
            link: entry.links().first().map(|link| link.href().to_string()),
        }
    }
}

impl From<&rss::Item> for IncomingEntry {
    fn from(entry: &rss::Item) -> Self {
        Self {
            title: entry.title().map(|entry_title| {
                let mut title = String::new();
                decode_html_entities_to_string(entry_title, &mut title);
                title
            }),
            author: entry.author().map(|entry_author| {
                let mut author = String::new();
                decode_html_entities_to_string(entry_author, &mut author);
                author
            }),
            pub_date: entry.pub_date().and_then(parse_datetime),
            description: entry.description().map(|entry_description| {
                let mut description = String::new();
                decode_html_entities_to_string(entry_description, &mut description);
                description
            }),
            content: entry.content().map(|entry_content| {
                let mut content = String::new();
                decode_html_entities_to_string(entry_content, &mut content);
                content
            }),
            link: entry.link().map(|link| link.to_owned()),
        }
    }
}

/// Metadata for an entry.
///
/// This type exists so we can load entry metadata for lots of
/// entries, without having to load all of the content for those entries,
/// as we only ever need an entry's content in memory when we are displaying
/// the currently selected entry.
#[derive(Clone, Debug)]
pub struct EntryMetadata {
    pub id: EntryId,
    pub feed_id: FeedId,
    pub title: Option<String>,
    pub author: Option<String>,
    pub pub_date: Option<chrono::DateTime<Utc>>,
    pub link: Option<String>,
    pub read_at: Option<chrono::DateTime<Utc>>,
    pub inserted_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

impl EntryMetadata {
    pub fn toggle_read(&self, conn: &rusqlite::Connection) -> Result<()> {
        if self.read_at.is_none() {
            self.mark_as_read(conn)
        } else {
            self.mark_as_unread(conn)
        }
    }

    fn mark_as_read(&self, conn: &rusqlite::Connection) -> Result<()> {
        let mut statement = conn.prepare("UPDATE entries SET read_at = ?2 WHERE id = ?1")?;
        statement.execute(params![self.id, Utc::now()])?;
        Ok(())
    }

    fn mark_as_unread(&self, conn: &rusqlite::Connection) -> Result<()> {
        let mut statement = conn.prepare("UPDATE entries SET read_at = NULL WHERE id = ?1")?;
        statement.execute([self.id])?;
        Ok(())
    }
}

pub struct EntryContent {
    pub content: Option<String>,
    pub description: Option<String>,
}

fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    diligent_date_parser::parse_date(s).map(|dt| dt.with_timezone(&Utc))
}

struct FeedAndEntries {
    pub feed: IncomingFeed,
    pub entries: Vec<IncomingEntry>,
}

impl FeedAndEntries {
    fn set_feed_link(&mut self, url: &str) {
        self.feed.feed_link = Some(url.to_owned());
    }

    fn set_latest_etag(&mut self, etag: Option<String>) {
        self.feed.latest_etag = etag;
    }
}

impl FromStr for FeedAndEntries {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match atom::Feed::from_str(s) {
            Ok(atom_feed) => {
                let feed = IncomingFeed {
                    title: Some(atom_feed.title.to_string()),
                    feed_link: None,
                    link: atom_feed.links.first().map(|link| link.href().to_string()),
                    feed_kind: FeedKind::Atom,
                    latest_etag: None,
                };

                let entries = atom_feed
                    .entries()
                    .iter()
                    .map(|entry| entry.into())
                    .collect::<Vec<_>>();

                Ok(FeedAndEntries { feed, entries })
            }

            Err(_e) => match Channel::from_str(s) {
                Ok(channel) => {
                    let feed = IncomingFeed {
                        title: Some(channel.title().to_string()),
                        feed_link: None,
                        link: Some(channel.link().to_string()),
                        feed_kind: FeedKind::Rss,
                        latest_etag: None,
                    };

                    let entries = channel
                        .items()
                        .iter()
                        .map(|item| item.into())
                        .collect::<Vec<_>>();

                    Ok(FeedAndEntries { feed, entries })
                }
                Err(e) => Err(e.into()),
            },
        }
    }
}

pub fn subscribe_to_feed(
    http_client: &ureq::Agent,
    conn: &mut rusqlite::Connection,
    url: &str,
) -> Result<FeedId> {
    let feed_and_entries = fetch_feed(http_client, url, None)?;

    match feed_and_entries {
        FeedResponse::CacheMiss(feed_and_entries) => {
            let feed_id = in_transaction(conn, |tx| {
                let feed_id = create_feed(tx, &feed_and_entries.feed).with_context(|| {
                    format!(
                        "creating feed {:?} failed",
                        &feed_and_entries.feed.feed_link
                    )
                })?;
                add_entries_to_feed(tx, feed_id, &feed_and_entries.entries).with_context(|| {
                    format!(
                        "inserting {} entries for feed {:?} failed",
                        &feed_and_entries.entries.len(),
                        &feed_and_entries.feed.feed_link
                    )
                })?;
                Ok(feed_id)
            })?;

            Ok(feed_id)
        }
        FeedResponse::CacheHit => {
            bail!("Did not expect feed to be cached in this instance as we did not pass an etag")
        }
    }
}

enum FeedResponse {
    /// The remote host returned a new feed.
    /// The data may not actually be new, as hosts
    /// seem to change etags for all kinds of reasons
    CacheMiss(FeedAndEntries),
    /// the remote host indicated a cache hit,
    /// and did not return any new data
    CacheHit,
}

fn fetch_feed(
    http_client: &ureq::Agent,
    url: &str,
    current_etag: Option<String>,
) -> Result<FeedResponse> {
    let request = http_client.get(url);

    let request = if let Some(etag) = current_etag {
        request.header("If-None-Match", &etag)
    } else {
        request
    };

    let mut response = request.call()?;

    match response.status() {
        // the etags did not match, it is a new feed file
        http::StatusCode::OK => {
            let etag_header = response
                .headers()
                .iter()
                .find(|header| header.0.as_str().to_lowercase() == "etag");

            let etag = etag_header.and_then(|etag_kv| match etag_kv.1.to_str() {
                Ok(str) => Some(str.to_owned()),
                Err(_) => None,
            });

            let content: String = response.body_mut().read_to_string()?;

            let mut feed_and_entries = FeedAndEntries::from_str(&content)?;

            feed_and_entries.set_latest_etag(etag);

            feed_and_entries.set_feed_link(url);

            Ok(FeedResponse::CacheMiss(feed_and_entries))
        }
        // the etags match, it is the same feed we already have
        http::StatusCode::NOT_MODIFIED => Ok(FeedResponse::CacheHit),
        _ => Err(anyhow::anyhow!(
            "received unexpected status code fetching feed {response:?}"
        )),
    }
}

/// fetches the feed and stores the new entries
/// uses the link as the uniqueness key.
/// TODO hash the content to see if anything changed, and update that way.
pub fn refresh_feed(
    client: &ureq::Agent,
    conn: &mut rusqlite::Connection,
    feed_id: FeedId,
) -> Result<()> {
    let feed_url = get_feed_url(conn, feed_id)
        .with_context(|| format!("Unable to get url for feed id {feed_id} from the database",))?;

    let current_etag = get_feed_latest_etag(conn, feed_id).with_context(|| {
        format!("Unable to get latest_etag for feed_id {feed_id} from the database")
    })?;

    let remote_feed = fetch_feed(client, &feed_url, current_etag)
        .with_context(|| format!("Failed to fetch feed {feed_url}"))?;

    if let FeedResponse::CacheMiss(remote_feed) = remote_feed {
        let remote_items = remote_feed.entries;
        let remote_items_links = remote_items
            .iter()
            .flat_map(|item| &item.link)
            .cloned()
            .collect::<HashSet<String>>();

        let local_entries_links = get_entries_links(conn, &ReadMode::All, feed_id)?
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>();

        let difference = remote_items_links
            .difference(&local_entries_links)
            .cloned()
            .collect::<HashSet<_>>();

        let items_to_add = remote_items
            .into_iter()
            .filter(|item| match &item.link {
                Some(link) => difference.contains(link.as_str()),
                None => false,
            })
            .collect::<Vec<_>>();

        in_transaction(conn, |tx| {
            add_entries_to_feed(tx, feed_id, &items_to_add)?;
            update_feed_refreshed_at(tx, feed_id)?;
            update_feed_etag(tx, feed_id, remote_feed.feed.latest_etag.clone())?;
            Ok(())
        })?;
    } else {
        in_transaction(conn, |tx| update_feed_refreshed_at(tx, feed_id))?;
    }

    Ok(())
}

pub fn initialize_db(conn: &mut rusqlite::Connection) -> Result<()> {
    in_transaction(conn, |tx| {
        let schema_version: u64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if schema_version == 0 {
            tx.pragma_update(None, "user_version", 1)?;

            tx.execute(
                "CREATE TABLE IF NOT EXISTS feeds (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        title TEXT,
        feed_link TEXT,
        link TEXT,
        feed_kind TEXT,
        refreshed_at TIMESTAMP,
        inserted_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
        updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
                [],
            )?;

            tx.execute(
                "CREATE TABLE IF NOT EXISTS entries (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        feed_id INTEGER,
        title TEXT,
        author TEXT,
        pub_date TIMESTAMP,
        description TEXT,
        content TEXT,
        link TEXT,
        read_at TIMESTAMP,
        inserted_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
        updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
                [],
            )?;

            tx.execute(
                "CREATE INDEX IF NOT EXISTS entries_feed_id_and_pub_date_and_inserted_at_index
        ON entries (feed_id, pub_date, inserted_at)",
                [],
            )?;
        }

        if schema_version <= 1 {
            tx.pragma_update(None, "user_version", 2)?;

            tx.execute("ALTER TABLE feeds ADD COLUMN latest_etag TEXT", [])?;
        }

        if schema_version <= 2 {
            tx.pragma_update(None, "user_version", 3)?;

            tx.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS feeds_feed_link ON feeds (feed_link)",
                [],
            )?;
        }

        Ok(())
    })
}

fn create_feed(tx: &rusqlite::Transaction, feed: &IncomingFeed) -> Result<FeedId> {
    let feed_id = tx.query_row::<FeedId, _, _>(
        "INSERT INTO feeds (title, link, feed_link, feed_kind)
        VALUES (?1, ?2, ?3, ?4)
        RETURNING id",
        params![feed.title, feed.link, feed.feed_link, feed.feed_kind],
        |r| r.get(0),
    )?;

    Ok(feed_id)
}

pub fn delete_feed(conn: &mut rusqlite::Connection, feed_id: FeedId) -> Result<()> {
    in_transaction(conn, |tx| {
        tx.execute("DELETE FROM feeds WHERE id = ?1", [feed_id])?;
        tx.execute("DELETE FROM entries WHERE feed_id = ?1", [feed_id])?;
        Ok(())
    })
}

fn add_entries_to_feed(
    tx: &rusqlite::Transaction,
    feed_id: FeedId,
    entries: &[IncomingEntry],
) -> Result<()> {
    if !entries.is_empty() {
        let now = Utc::now();

        let mut insert_statement = tx.prepare(
            "INSERT INTO entries (feed_id, title, author, pub_date, description, content, link, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )?;

        // in most databases, doing this kind of "multiple inserts in a loop" thing would be bad and slow, but it's ok here because:
        // 1. it is within single a transaction. in SQLite, doing many writes in the same transaction is actually fast
        // 2. it is with single prepared statement, which further improves its write throughput
        // see further: https://stackoverflow.com/questions/1711631/improve-insert-per-second-performance-of-sqlite
        for entry in entries {
            insert_statement.execute(params![
                feed_id,
                entry.title,
                entry.author,
                entry.pub_date,
                entry.description,
                entry.content,
                entry.link,
                now
            ])?;
        }
    }

    Ok(())
}

pub fn get_feed(conn: &rusqlite::Connection, feed_id: FeedId) -> Result<Feed> {
    let s = conn.query_row(
        "SELECT id, title, feed_link, link, feed_kind, refreshed_at, inserted_at, updated_at, latest_etag FROM feeds WHERE id=?1",
        [feed_id],
        |row| {
            let feed_kind_str: String = row.get(4)?;
            let feed_kind: FeedKind = FeedKind::from_str(&feed_kind_str)
                .unwrap_or_else(|_| panic!("FeedKind must be Atom or RSS, got {feed_kind_str}"));

            Ok(Feed {
                id: row.get(0)?,
                title: row.get(1)?,
                feed_link: row.get(2)?,
                link: row.get(3)?,
                feed_kind,
                refreshed_at: row.get(5)?,
                inserted_at: row.get(6)?,
                updated_at: row.get(7)?,
                latest_etag: row.get(8)?,
            })
        },
    )?;

    Ok(s)
}

fn update_feed_refreshed_at(tx: &rusqlite::Transaction, feed_id: FeedId) -> Result<()> {
    tx.execute(
        "UPDATE feeds SET refreshed_at = ?2 WHERE id = ?1",
        params![feed_id, Utc::now()],
    )?;

    Ok(())
}

fn update_feed_etag(
    tx: &rusqlite::Transaction,
    feed_id: FeedId,
    latest_etag: Option<String>,
) -> Result<()> {
    tx.execute(
        "UPDATE feeds SET latest_etag = ?2 WHERE id = ?1",
        params![feed_id, latest_etag],
    )?;

    Ok(())
}

pub fn get_feed_url(conn: &rusqlite::Connection, feed_id: FeedId) -> Result<String> {
    let s: String = conn.query_row(
        "SELECT feed_link FROM feeds WHERE id=?1",
        [feed_id],
        |row| row.get(0),
    )?;

    Ok(s)
}

fn get_feed_latest_etag(conn: &rusqlite::Connection, feed_id: FeedId) -> Result<Option<String>> {
    let s: Option<String> = conn.query_row(
        "SELECT latest_etag FROM feeds WHERE id=?1",
        [feed_id],
        |row| {
            let etag: Option<String> = row.get(0)?;
            Ok(etag)
        },
    )?;

    Ok(s)
}

pub fn get_feeds(conn: &rusqlite::Connection) -> Result<Vec<Feed>> {
    let mut statement = conn.prepare(
        "SELECT 
          id, 
          title, 
          feed_link, 
          link, 
          feed_kind, 
          refreshed_at, 
          inserted_at, 
          updated_at,
          latest_etag
        FROM feeds ORDER BY lower(title) ASC",
    )?;
    let mut feeds = vec![];
    for feed in statement.query_map([], |row| {
        Ok(Feed {
            id: row.get(0)?,
            title: row.get(1)?,
            feed_link: row.get(2)?,
            link: row.get(3)?,
            feed_kind: row.get(4)?,
            refreshed_at: row.get(5)?,
            inserted_at: row.get(6)?,
            updated_at: row.get(7)?,
            latest_etag: row.get(8)?,
        })
    })? {
        feeds.push(feed?)
    }

    Ok(feeds)
}

pub fn get_feed_ids(conn: &rusqlite::Connection) -> Result<Vec<FeedId>> {
    let mut statement = conn.prepare("SELECT id FROM feeds ORDER BY lower(title) ASC")?;
    let mut ids = vec![];
    for id in statement.query_map([], |row| row.get(0))? {
        ids.push(id?)
    }

    Ok(ids)
}

pub fn get_entry_meta(conn: &rusqlite::Connection, entry_id: EntryId) -> Result<EntryMetadata> {
    let result = conn.query_row(
        "SELECT 
          id, 
          feed_id, 
          title, 
          author, 
          pub_date, 
          link, 
          read_at, 
          inserted_at, 
          updated_at 
        FROM entries WHERE id=?1",
        [entry_id],
        |row| {
            Ok(EntryMetadata {
                id: row.get(0)?,
                feed_id: row.get(1)?,
                title: row.get(2)?,
                author: row.get(3)?,
                pub_date: row.get(4)?,
                link: row.get(5)?,
                read_at: row.get(6)?,
                inserted_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        },
    )?;

    Ok(result)
}

pub fn get_entry_content(conn: &rusqlite::Connection, entry_id: EntryId) -> Result<EntryContent> {
    let result = conn.query_row(
        "SELECT content, description FROM entries WHERE id=?1",
        [entry_id],
        |row| {
            Ok(EntryContent {
                content: row.get(0)?,
                description: row.get(1)?,
            })
        },
    )?;

    Ok(result)
}

pub fn get_entries_metas(
    conn: &rusqlite::Connection,
    read_mode: &ReadMode,
    feed_id: FeedId,
) -> Result<Vec<EntryMetadata>> {
    let read_at_predicate = match read_mode {
        ReadMode::ShowUnread => "\nAND read_at IS NULL",
        ReadMode::ShowRead => "\nAND read_at IS NOT NULL",
        ReadMode::All => "\n",
    };

    // we get weird pubDate formats from feeds,
    // so sort by inserted at as this as a stable order at least
    let mut query = "SELECT 
        id, 
        feed_id, 
        title, 
        author, 
        pub_date, 
        link, 
        read_at, 
        inserted_at, 
        updated_at 
        FROM entries 
        WHERE feed_id=?1"
        .to_string();

    query.push_str(read_at_predicate);
    query.push_str("\nORDER BY pub_date DESC, inserted_at DESC");

    let mut statement = conn.prepare(&query)?;
    let mut entries = vec![];
    for entry in statement.query_map([feed_id], |row| {
        Ok(EntryMetadata {
            id: row.get(0)?,
            feed_id: row.get(1)?,
            title: row.get(2)?,
            author: row.get(3)?,
            pub_date: row.get(4)?,
            link: row.get(5)?,
            read_at: row.get(6)?,
            inserted_at: row.get(7)?,
            updated_at: row.get(8)?,
        })
    })? {
        entries.push(entry?)
    }

    Ok(entries)
}

pub fn get_entries_links(
    conn: &rusqlite::Connection,
    read_mode: &ReadMode,
    feed_id: FeedId,
) -> Result<Vec<Option<String>>> {
    let read_at_predicate = match read_mode {
        ReadMode::ShowUnread => "\nAND read_at IS NULL",
        ReadMode::ShowRead => "\nAND read_at IS NOT NULL",
        ReadMode::All => "\n",
    };

    // we get weird pubDate formats from feeds,
    // so sort by inserted at as this as a stable order at least
    let mut query = "SELECT link FROM entries WHERE feed_id=?1".to_string();

    query.push_str(read_at_predicate);
    query.push_str("\nORDER BY pub_date DESC, inserted_at DESC");

    let mut links = vec![];
    let mut statement = conn.prepare(&query)?;

    for link in statement.query_map([feed_id], |row| row.get(0))? {
        links.push(link?);
    }

    Ok(links)
}

/// run `f` in a transaction, committing if `f` returns an `Ok` value,
/// otherwise rolling back.
fn in_transaction<F, R>(conn: &mut rusqlite::Connection, f: F) -> Result<R>
where
    F: Fn(&rusqlite::Transaction) -> Result<R>,
{
    let tx = conn.transaction()?;

    let result = f(&tx)?;

    tx.commit()?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    const ZCT: &str = "https://zeroclarkthirty.com/feed";

    #[test]
    fn it_fetches() {
        let http_client = ureq::Agent::config_builder()
            .timeout_recv_body(Some(std::time::Duration::from_secs(5)))
            .build()
            .into();
        let feed_and_entries = fetch_feed(&http_client, ZCT, None).unwrap();
        if let FeedResponse::CacheMiss(feed_and_entries) = feed_and_entries {
            assert!(!feed_and_entries.entries.is_empty())
        } else {
            panic!("somehow got a cached response when passing no etag")
        }
    }

    #[test]
    fn it_subscribes_to_a_feed() {
        let http_client = ureq::Agent::config_builder()
            .timeout_recv_body(Some(std::time::Duration::from_secs(5)))
            .build()
            .into();
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        initialize_db(&mut conn).unwrap();
        subscribe_to_feed(&http_client, &mut conn, ZCT).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();

        assert!(count > 50)
    }

    #[test]
    fn refresh_feed_does_not_add_any_items_if_there_are_no_new_items() {
        let http_client = ureq::Agent::config_builder()
            .timeout_recv_body(Some(std::time::Duration::from_secs(5)))
            .build()
            .into();
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        initialize_db(&mut conn).unwrap();
        subscribe_to_feed(&http_client, &mut conn, ZCT).unwrap();
        let feed_id = 1.into();
        let old_entries = get_entries_metas(&conn, &ReadMode::ShowUnread, feed_id).unwrap();
        refresh_feed(&http_client, &mut conn, feed_id).unwrap();
        let e = get_entry_meta(&conn, 1.into()).unwrap();
        e.mark_as_read(&conn).unwrap();
        let new_entries = get_entries_metas(&conn, &ReadMode::ShowUnread, feed_id).unwrap();

        assert_eq!(new_entries.len(), old_entries.len() - 1);
    }

    #[test]
    fn works_transactionally() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();

        conn.execute("CREATE TABLE foo (t)", []).unwrap();

        let count: i64 = conn
            .query_row("select count(*) from foo", [], |row| row.get(0))
            .unwrap();

        // should be nothing in the table
        assert_eq!(count, 0);

        // insert one row to prove it works
        let _ = in_transaction(&mut conn, |tx| {
            tx.execute(r#"INSERT INTO foo (t) values ("some initial string")"#, [])?;
            Ok(())
        });

        let count: i64 = conn
            .query_row("select count(*) from foo", [], |row| row.get(0))
            .unwrap();

        // we inserted one row, there should be one
        assert_eq!(count, 1);

        // do 2 inserts in the same way as before, but error in the middle of the inserts.
        // this should rollback
        let tr = in_transaction(&mut conn, |tx| {
            tx.execute(r#"INSERT INTO foo (t) values ("some string")"#, [])?;
            tx.execute("this is not valid sql, it should error and rollback", [])?;
            tx.execute(r#"INSERT INTO foo (t) values ("some other string")"#, [])?;

            Ok(())
        });

        // it should be an error
        let e = tr.unwrap_err();
        assert!(e.to_string().contains("syntax error"));

        let count: i64 = conn
            .query_row("select count(*) from foo", [], |row| row.get(0))
            .unwrap();

        // assert that no further entries have been inserted
        assert_eq!(count, 1);
    }
}
