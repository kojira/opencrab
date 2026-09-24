use super::Migration;
use rusqlite::Connection;

pub(super) const MIGRATIONS: &[Migration] = &[Migration {
    version: 57,
    description: "generic two-ledger delivery evidence",
    up: migrate_v57,
}];

fn migrate_v57(conn: &Connection) -> rusqlite::Result<()> {
    for sql in [
        "ALTER TABLE deliveries ADD COLUMN payload_digest TEXT",
        "ALTER TABLE deliveries ADD COLUMN delivery_guarantee TEXT NOT NULL DEFAULT 'legacy_unqualified'",
        "ALTER TABLE deliveries ADD COLUMN prepared_protocol_digest TEXT",
        "ALTER TABLE deliveries ADD COLUMN acknowledged_at INTEGER",
        "ALTER TABLE deliveries ADD COLUMN frame_kind TEXT NOT NULL DEFAULT 'say' CHECK(frame_kind IN ('say','invoke'))",
        "ALTER TABLE deliveries ADD COLUMN prepared_frame_json TEXT CHECK(prepared_frame_json IS NULL OR json_valid(prepared_frame_json))",
    ] {
        conn.execute_batch(sql)?;
    }
    conn.execute_batch(
        r#"
        CREATE TRIGGER deliveries_s7_immutable_evidence
        BEFORE UPDATE OF binding_id, payload_json, payload_digest, delivery_guarantee,
                         prepared_protocol_digest, frame_kind, prepared_frame_json
        ON deliveries
        BEGIN
          SELECT RAISE(ABORT, 'delivery evidence is immutable')
          WHERE NEW.binding_id IS NOT OLD.binding_id
             OR NEW.payload_json IS NOT OLD.payload_json
             OR NEW.payload_digest IS NOT OLD.payload_digest
             OR NEW.delivery_guarantee IS NOT OLD.delivery_guarantee
             OR NEW.prepared_protocol_digest IS NOT OLD.prepared_protocol_digest
             OR NEW.frame_kind IS NOT OLD.frame_kind
             OR NEW.prepared_frame_json IS NOT OLD.prepared_frame_json;
        END;
        CREATE INDEX idx_deliveries_s7_pending
          ON deliveries(binding_id, created_at, delivery_id)
          WHERE state='sending';
        "#,
    )
}
