//! Operator tool for gate-admin principals and subject-association grants (Issue #1070).
//!
//! Writes go straight to the core database through the same sealed-principal and grant
//! primitives core uses, so a running core honors a new principal on its next request
//! without a restart. No gate-admin HTTP route is added.
//!
//! ```text
//! opencrab-gate-admin --db <core.db> principal-issue --principal-id <id> \
//!     --operation instance.read [--operation ...] --subject <n> [--subject ...] \
//!     (--instance <uuid> ... | --creation-namespace <uuid>) \
//!     --expires-at <RFC3339-UTC> --credential-out <new-file>
//! opencrab-gate-admin --db <core.db> principal-revoke --principal-id <id>
//! opencrab-gate-admin --db <core.db> grant-issue --agent-id <id> --subject <n> \
//!     --ttl-secs <1..=3600> --grant-out <new-file>
//! opencrab-gate-admin instance-id --creation-namespace <uuid> --agent-id <id>
//! ```
//!
//! Secrets are written only to a newly created mode-0600 file (never stdout or argv) and
//! the tool refuses to open a database whose schema version differs from this binary.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use opencrab_extgate::gate_admin_security::{
    issue_principal, issue_subject_grant, namespace_instance_id, revoke, IssuedCredential,
    Operation, PrincipalRequest, PrincipalScope,
};
use rusqlite::{Connection, OpenFlags};
use uuid::Uuid;

#[derive(Default)]
struct Args {
    values: Vec<(String, String)>,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Self> {
        let mut values = Vec::new();
        let mut iter = raw.iter();
        while let Some(flag) = iter.next() {
            let name = flag
                .strip_prefix("--")
                .ok_or_else(|| anyhow!("unexpected argument: {flag}"))?;
            let value = iter
                .next()
                .ok_or_else(|| anyhow!("missing value for --{name}"))?;
            values.push((name.to_owned(), value.clone()));
        }
        Ok(Self { values })
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn one(&self, name: &str) -> Result<&str> {
        match self.all(name).as_slice() {
            [value] => Ok(value),
            [] => bail!("--{name} is required"),
            _ => bail!("--{name} must be given once"),
        }
    }

    fn optional(&self, name: &str) -> Result<Option<&str>> {
        match self.all(name).as_slice() {
            [] => Ok(None),
            [value] => Ok(Some(value)),
            _ => bail!("--{name} must be given at most once"),
        }
    }

    fn only(&self, allowed: &[&str]) -> Result<()> {
        match self
            .values
            .iter()
            .find(|(key, _)| !allowed.contains(&key.as_str()))
        {
            Some((key, _)) => bail!("unknown option --{key}"),
            None => Ok(()),
        }
    }
}

fn canonical_uuid(value: &str) -> Result<Uuid> {
    let uuid = Uuid::parse_str(value).with_context(|| format!("invalid UUID: {value}"))?;
    if uuid.hyphenated().to_string() != value {
        bail!("UUID must be canonical lowercase hyphenated: {value}");
    }
    Ok(uuid)
}

fn unique<T: Ord>(values: impl IntoIterator<Item = Result<T>>, what: &str) -> Result<BTreeSet<T>> {
    let mut set = BTreeSet::new();
    for value in values {
        if !set.insert(value?) {
            bail!("duplicate {what}");
        }
    }
    Ok(set)
}

fn now_nanos() -> i64 {
    opencrab_extgate::now_nanos()
}

/// Opens an existing core database without creating it and without running migrations.
fn open_core_db(path: &str) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open existing core database: {path}"))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let supported = opencrab_db::schema::supported_schema_version();
    if version != supported {
        bail!("core schema version {version} does not match this tool ({supported})");
    }
    Ok(conn)
}

/// Creates the secret file exclusively (never follows or replaces an existing path).
fn create_secret_file(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("create new secret file {}", path.display()))
}

fn write_secret(mut file: std::fs::File, secret: &IssuedCredential) -> Result<()> {
    file.write_all(secret.expose_secret().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn principal_issue(db: &str, args: &Args) -> Result<String> {
    args.only(&[
        "principal-id",
        "operation",
        "subject",
        "instance",
        "creation-namespace",
        "expires-at",
        "credential-out",
    ])?;
    let principal_id = args.one("principal-id")?.to_owned();
    let operations = unique(
        args.all("operation").into_iter().map(|name| {
            Operation::from_name(name).ok_or_else(|| anyhow!("unknown operation: {name}"))
        }),
        "operation",
    )?;
    let subject_ids = unique(
        args.all("subject").into_iter().map(|value| {
            value
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| anyhow!("subject must be a positive integer: {value}"))
        }),
        "subject",
    )?;
    let instances = unique(
        args.all("instance").into_iter().map(canonical_uuid),
        "instance",
    )?;
    let scope = match (instances.is_empty(), args.optional("creation-namespace")?) {
        (false, None) => PrincipalScope::Exact(instances),
        (true, Some(namespace)) => PrincipalScope::CreationNamespace(canonical_uuid(namespace)?),
        _ => bail!("give either --instance (one or more) or --creation-namespace, not both"),
    };
    let expires_at = chrono::DateTime::parse_from_rfc3339(args.one("expires-at")?)
        .context("--expires-at must be RFC3339")?
        .timestamp_nanos_opt()
        .ok_or_else(|| anyhow!("--expires-at out of range"))?;
    let out = PathBuf::from(args.one("credential-out")?);
    let request = PrincipalRequest {
        principal_id: principal_id.clone(),
        operations,
        subject_ids,
        scope,
        expires_at,
    };
    let mut conn = open_core_db(db)?;
    let file = create_secret_file(&out)?;
    let issued = match issue_principal(&mut conn, &request, now_nanos()) {
        Ok(issued) => issued,
        Err(error) => {
            let _ = std::fs::remove_file(&out);
            bail!("principal issue refused: {error}");
        }
    };
    if let Err(error) = write_secret(file, &issued) {
        // The bearer is unrecoverable; revoke so no unknown credential stays live.
        let _ = revoke(&conn, &principal_id, now_nanos());
        let _ = std::fs::remove_file(&out);
        return Err(error.context("credential write failed; principal revoked"));
    }
    Ok(format!(
        "issued principal {principal_id}; bearer written to {}",
        out.display()
    ))
}

fn principal_revoke(db: &str, args: &Args) -> Result<String> {
    args.only(&["principal-id"])?;
    let principal_id = args.one("principal-id")?;
    let conn = open_core_db(db)?;
    revoke(&conn, principal_id, now_nanos())
        .map_err(|error| anyhow!("principal revoke refused: {error}"))?;
    Ok(format!("revoked principal {principal_id}"))
}

fn grant_issue(db: &str, args: &Args) -> Result<String> {
    args.only(&["agent-id", "subject", "ttl-secs", "grant-out"])?;
    let agent_id = args.one("agent-id")?;
    let subject_id: i64 = args.one("subject")?.parse().context("--subject")?;
    let ttl_secs: i64 = args.one("ttl-secs")?.parse().context("--ttl-secs")?;
    let ttl = ttl_secs
        .checked_mul(1_000_000_000)
        .ok_or_else(|| anyhow!("--ttl-secs out of range"))?;
    let out = PathBuf::from(args.one("grant-out")?);
    let mut conn = open_core_db(db)?;
    let file = create_secret_file(&out)?;
    let issued = match issue_subject_grant(&mut conn, agent_id, subject_id, ttl, now_nanos()) {
        Ok(issued) => issued,
        Err(error) => {
            let _ = std::fs::remove_file(&out);
            bail!("grant issue refused: {error}");
        }
    };
    if let Err(error) = write_secret(file, &issued) {
        let _ = std::fs::remove_file(&out);
        return Err(error.context("grant write failed; the unwritten grant expires unused"));
    }
    Ok(format!(
        "issued single-use grant for subject {subject_id}; written to {}",
        out.display()
    ))
}

fn instance_id(args: &Args) -> Result<String> {
    args.only(&["creation-namespace", "agent-id"])?;
    let namespace = canonical_uuid(args.one("creation-namespace")?)?;
    Ok(namespace_instance_id(&namespace, args.one("agent-id")?).to_string())
}

fn run(raw: &[String]) -> Result<String> {
    let (db, rest) = match raw {
        [flag, db, rest @ ..] if flag == "--db" => (Some(db.as_str()), rest),
        _ => (None, raw),
    };
    let (command, options) = rest
        .split_first()
        .ok_or_else(|| anyhow!("missing command"))?;
    let args = Args::parse(options)?;
    let require_db = || db.ok_or_else(|| anyhow!("--db <core database> is required"));
    match command.as_str() {
        "principal-issue" => principal_issue(require_db()?, &args),
        "principal-revoke" => principal_revoke(require_db()?, &args),
        "grant-issue" => grant_issue(require_db()?, &args),
        "instance-id" => instance_id(&args),
        other => bail!("unknown command: {other}"),
    }
}

fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    println!("{}", run(&raw)?);
    Ok(())
}

#[cfg(test)]
#[path = "../gate_admin_cli_tests.rs"]
mod tests;
