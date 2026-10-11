//! v59: put Memory Index topics under the month they happened in (D-1078).
//!
//! Before this version the index builder reused one session node per conversation
//! session, parented under the month of the session's first batch. A long-lived session
//! (e.g. a Nostr session) therefore kept attaching later months' topics to that first month.
//! The month line then grew every turn and the finished month was re-rolled again and again.
//!
//! This migration moves every dated topic (`date_from`, the same field the builder now uses)
//! to a session node under its own month: `session-{agent}-{session}-{YYYY-MM}`. The legacy
//! un-suffixed session node stays as the node for the month it is already under. Missing
//! period nodes are created. Every period that lost or gained topics gets
//! `summary_refreshed_at = NULL`, so the normal rollup pass regenerates its summary (no LLM
//! call here). Session and period nodes left without children are removed. Topics without a
//! usable `date_from` are left where they are.
//!
//! Idempotent: a second run finds no topic whose month differs from its period's title.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{params, Connection, OptionalExtension};

use super::super::helpers::table_exists;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 59,
    description: "re-parent memory index topics to the month they happened in",
    up: reparent_topics_to_own_month,
}];

/// (agent_id, source session id, `YYYY-MM`): one session node per session and month.
type MonthTarget = (String, String, String);

/// One topic whose `date_from` month differs from its period's title.
struct MisplacedTopic {
    topic_id: String,
    agent_id: String,
    month: String,
    old_session_id: String,
    old_period_id: String,
    root_id: Option<String>,
    source_session_id: String,
    start_log_id: Option<i64>,
    end_log_id: Option<i64>,
}

fn reparent_topics_to_own_month(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "memory_index_nodes")? {
        return Ok(());
    }
    let misplaced = find_misplaced_topics(conn)?;
    if misplaced.is_empty() {
        return Ok(());
    }
    let has_fts = table_exists(conn, "memory_index_fts")?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut short_ids = ShortIdAllocator::default();
    // (agent_id, node id) of every session / period node that gained or lost topics.
    let mut touched_periods: BTreeSet<(String, String)> = BTreeSet::new();
    let mut touched_sessions: BTreeSet<(String, String)> = BTreeSet::new();

    // (agent, session, month) -> log range of the topics that move there.
    let mut targets: BTreeMap<MonthTarget, (Option<i64>, Option<i64>)> = BTreeMap::new();
    for t in &misplaced {
        let range = targets
            .entry((
                t.agent_id.clone(),
                t.source_session_id.clone(),
                t.month.clone(),
            ))
            .or_insert((None, None));
        range.0 = min_opt(range.0, t.start_log_id);
        range.1 = max_opt(range.1, t.end_log_id);
    }

    let mut example_for_target: HashMap<MonthTarget, &MisplacedTopic> = HashMap::new();
    for t in &misplaced {
        example_for_target
            .entry((
                t.agent_id.clone(),
                t.source_session_id.clone(),
                t.month.clone(),
            ))
            .or_insert(t);
    }
    let mut session_for_target: HashMap<MonthTarget, String> = HashMap::new();
    for (key, (start, end)) in &targets {
        let (agent_id, source_session_id, month) = key;
        let example = example_for_target[key];
        let period_id = ensure_period(
            conn,
            &mut short_ids,
            has_fts,
            &now,
            agent_id,
            month,
            example.root_id.as_deref(),
        )?;
        let session_id = ensure_session(
            conn,
            &mut short_ids,
            has_fts,
            &now,
            SessionTarget {
                agent_id,
                source_session_id,
                month,
                period_id: &period_id,
                template_id: &example.old_session_id,
                start_log_id: *start,
                end_log_id: *end,
            },
        )?;
        touched_periods.insert((agent_id.clone(), period_id));
        touched_sessions.insert((agent_id.clone(), session_id.clone()));
        session_for_target.insert(
            (agent_id.clone(), source_session_id.clone(), month.clone()),
            session_id,
        );
    }

    let mut update = conn
        .prepare("UPDATE memory_index_nodes SET parent_id = ?1, updated_at = ?2 WHERE id = ?3")?;
    for t in &misplaced {
        let key = (
            t.agent_id.clone(),
            t.source_session_id.clone(),
            t.month.clone(),
        );
        let session_id = &session_for_target[&key];
        update.execute(params![session_id, now, t.topic_id])?;
        touched_sessions.insert((t.agent_id.clone(), t.old_session_id.clone()));
        touched_periods.insert((t.agent_id.clone(), t.old_period_id.clone()));
    }
    drop(update);

    for (agent_id, session_id) in &touched_sessions {
        remove_if_childless(conn, has_fts, agent_id, session_id)?;
    }
    for (agent_id, period_id) in &touched_periods {
        conn.execute(
            "UPDATE memory_index_nodes SET summary_refreshed_at = NULL, updated_at = ?1
             WHERE id = ?2",
            params![now, period_id],
        )?;
        remove_if_childless(conn, has_fts, agent_id, period_id)?;
    }
    for (agent_id, id) in touched_sessions.iter().chain(touched_periods.iter()) {
        conn.execute(
            "UPDATE memory_index_nodes
             SET child_count = (SELECT COUNT(*) FROM memory_index_nodes c
                                WHERE c.agent_id = ?1 AND c.parent_id = ?2)
             WHERE id = ?2",
            params![agent_id, id],
        )?;
    }
    Ok(())
}

fn find_misplaced_topics(conn: &Connection) -> rusqlite::Result<Vec<MisplacedTopic>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.agent_id, substr(t.date_from, 1, 7), s.id, p.id, p.parent_id,
                s.source_session_id, t.start_log_id, t.end_log_id
         FROM memory_index_nodes t
         JOIN memory_index_nodes s ON s.id = t.parent_id AND s.node_type = 'session'
         JOIN memory_index_nodes p ON p.id = s.parent_id AND p.node_type = 'period'
         WHERE t.node_type = 'topic' AND t.source_type = 'session_log'
           AND p.source_type = 'session_log'
           AND s.source_session_id IS NOT NULL
           AND t.date_from GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]*'
           AND substr(t.date_from, 1, 7) != p.title
         ORDER BY t.agent_id, t.start_log_id, t.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(MisplacedTopic {
            topic_id: r.get(0)?,
            agent_id: r.get(1)?,
            month: r.get(2)?,
            old_session_id: r.get(3)?,
            old_period_id: r.get(4)?,
            root_id: r.get(5)?,
            source_session_id: r.get(6)?,
            start_log_id: r.get(7)?,
            end_log_id: r.get(8)?,
        })
    })?;
    rows.collect()
}

/// Hands out short ids per (agent, prefix) the same way `queries::next_short_id` does
/// (max numeric suffix + 1), reading the current max once per pair.
#[derive(Default)]
struct ShortIdAllocator {
    next: HashMap<(String, &'static str), i64>,
}

impl ShortIdAllocator {
    fn allocate(
        &mut self,
        conn: &Connection,
        agent_id: &str,
        prefix: &'static str,
    ) -> rusqlite::Result<String> {
        let key = (agent_id.to_string(), prefix);
        let next = match self.next.get(&key) {
            Some(n) => *n,
            None => {
                let max: Option<i64> = conn.query_row(
                    "SELECT MAX(CAST(SUBSTR(short_id, ?3) AS INTEGER)) FROM memory_index_nodes
                     WHERE agent_id = ?1 AND short_id LIKE ?2",
                    params![agent_id, format!("{prefix}%"), (prefix.len() + 1) as i64],
                    |r| r.get(0),
                )?;
                max.unwrap_or(0) + 1
            }
        };
        self.next.insert(key, next + 1);
        Ok(format!("{prefix}{next}"))
    }
}

fn ensure_period(
    conn: &Connection,
    short_ids: &mut ShortIdAllocator,
    has_fts: bool,
    now: &str,
    agent_id: &str,
    month: &str,
    root_id: Option<&str>,
) -> rusqlite::Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM memory_index_nodes
             WHERE agent_id = ?1 AND node_type = 'period' AND source_type = 'session_log'
               AND title = ?2
             ORDER BY id LIMIT 1",
            params![agent_id, month],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = format!("period-{agent_id}-{month}");
    let short_id = short_ids.allocate(conn, agent_id, "p")?;
    let summary = format!("Conversations from {month}");
    conn.execute(
        "INSERT INTO memory_index_nodes
           (id, agent_id, parent_id, node_type, source_type, title, summary, depth,
            child_count, token_count, created_at, updated_at, short_id, keywords_json)
         VALUES (?1, ?2, ?3, 'period', 'session_log', ?4, ?5, 1, 0, 0, ?6, ?6, ?7, '[]')",
        params![id, agent_id, root_id, month, summary, now, short_id],
    )?;
    if has_fts {
        insert_fts(conn, &id, agent_id, "period", month, &summary)?;
    }
    Ok(id)
}

struct SessionTarget<'a> {
    agent_id: &'a str,
    source_session_id: &'a str,
    month: &'a str,
    period_id: &'a str,
    /// The misplaced session node; its title and summary are copied to the new node.
    template_id: &'a str,
    start_log_id: Option<i64>,
    end_log_id: Option<i64>,
}

fn ensure_session(
    conn: &Connection,
    short_ids: &mut ShortIdAllocator,
    has_fts: bool,
    now: &str,
    target: SessionTarget<'_>,
) -> rusqlite::Result<String> {
    let SessionTarget {
        agent_id,
        source_session_id,
        month,
        period_id,
        template_id,
        start_log_id,
        end_log_id,
    } = target;
    // A session node of this session already under the right month (the legacy un-suffixed
    // node, or a month node from an earlier build) receives the topics.
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM memory_index_nodes
             WHERE agent_id = ?1 AND node_type = 'session' AND source_session_id = ?2
               AND parent_id = ?3
             ORDER BY id LIMIT 1",
            params![agent_id, source_session_id, period_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = format!("session-{agent_id}-{source_session_id}-{month}");
    let (title, summary): (String, String) = conn.query_row(
        "SELECT title, summary FROM memory_index_nodes WHERE id = ?1",
        params![template_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let short_id = short_ids.allocate(conn, agent_id, "s")?;
    conn.execute(
        "INSERT INTO memory_index_nodes
           (id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id,
            end_log_id, source_session_id, depth, child_count, token_count, created_at,
            updated_at, short_id, keywords_json)
         VALUES (?1, ?2, ?3, 'session', 'session_log', ?4, ?5, ?6, ?7, ?8, 2, 0, 0, ?9, ?9,
                 ?10, '[]')",
        params![
            id,
            agent_id,
            period_id,
            title,
            summary,
            start_log_id,
            end_log_id,
            source_session_id,
            now,
            short_id
        ],
    )?;
    if has_fts {
        insert_fts(conn, &id, agent_id, "session", &title, &summary)?;
    }
    Ok(id)
}

fn insert_fts(
    conn: &Connection,
    node_id: &str,
    agent_id: &str,
    node_type: &str,
    title: &str,
    summary: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_index_fts
           (title, summary, keywords, node_id, agent_id, node_type, source_type)
         VALUES (?1, ?2, '', ?3, ?4, ?5, 'session_log')",
        params![title, summary, node_id, agent_id, node_type],
    )?;
    Ok(())
}

fn remove_if_childless(
    conn: &Connection,
    has_fts: bool,
    agent_id: &str,
    id: &str,
) -> rusqlite::Result<()> {
    let has_children: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_index_nodes WHERE agent_id = ?1 AND parent_id = ?2)",
        params![agent_id, id],
        |r| r.get(0),
    )?;
    if has_children {
        return Ok(());
    }
    if has_fts {
        conn.execute(
            "DELETE FROM memory_index_fts WHERE node_id = ?1",
            params![id],
        )?;
    }
    conn.execute("DELETE FROM memory_index_nodes WHERE id = ?1", params![id])?;
    Ok(())
}

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}
