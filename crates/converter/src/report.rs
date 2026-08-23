use crate::source::{SourceRow, SourceTable};
use crate::{ConverterError, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ContributionKey {
    source_table: String,
    source_key: Vec<u8>,
    row_digest: [u8; 32],
}

impl ContributionKey {
    pub(crate) fn new(table: &SourceTable, row: &SourceRow) -> Self {
        Self {
            source_table: table.name.into(),
            source_key: row.source_key.clone(),
            row_digest: row.row_digest,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassAccounting {
    pub source_table: String,
    pub logical_class: String,
    pub source_rows: u64,
    pub canonical_outcomes: u64,
    pub raw_outcomes: u64,
    pub dropped_outcomes: u64,
    pub exact_one_violations: u64,
    pub physical_rows: BTreeMap<String, u64>,
    expected: BTreeMap<ContributionKey, u64>,
    canonical: BTreeMap<ContributionKey, u64>,
    raw: BTreeMap<ContributionKey, u64>,
    dropped: BTreeMap<ContributionKey, u64>,
    streaming: bool,
}

impl ClassAccounting {
    pub(crate) fn new(
        source_table: impl Into<String>,
        logical_class: impl Into<String>,
        contributions: impl IntoIterator<Item = ContributionKey>,
        physical_rows: BTreeMap<String, u64>,
    ) -> Self {
        let mut expected = BTreeMap::new();
        for contribution in contributions {
            *expected.entry(contribution).or_default() += 1;
        }
        let source_rows = expected.values().sum();
        Self {
            source_table: source_table.into(),
            logical_class: logical_class.into(),
            source_rows,
            canonical_outcomes: 0,
            raw_outcomes: 0,
            dropped_outcomes: 0,
            exact_one_violations: 0,
            physical_rows,
            expected,
            canonical: BTreeMap::new(),
            raw: BTreeMap::new(),
            dropped: BTreeMap::new(),
            streaming: false,
        }
    }

    pub(crate) fn streaming(
        source_table: impl Into<String>,
        logical_class: impl Into<String>,
        physical_rows: BTreeMap<String, u64>,
    ) -> Self {
        let mut value = Self::new(source_table, logical_class, [], physical_rows);
        value.streaming = true;
        value
    }

    pub(crate) fn canonical_streamed(&mut self) {
        self.source_rows += 1;
        self.canonical_outcomes += 1;
    }

    pub(crate) fn raw_streamed(&mut self) {
        self.source_rows += 1;
        self.raw_outcomes += 1;
    }

    pub(crate) fn dropped_streamed(&mut self) {
        self.source_rows += 1;
        self.dropped_outcomes += 1;
    }

    pub(crate) fn canonical(&mut self, contribution: ContributionKey) {
        *self.canonical.entry(contribution).or_default() += 1;
        self.canonical_outcomes += 1;
    }

    pub(crate) fn raw(&mut self, contribution: ContributionKey) {
        *self.raw.entry(contribution).or_default() += 1;
        self.raw_outcomes += 1;
    }

    pub(crate) fn verify(&mut self) {
        if self.streaming {
            self.exact_one_violations = self
                .source_rows
                .abs_diff(self.canonical_outcomes + self.raw_outcomes + self.dropped_outcomes);
            return;
        }
        let keys = self
            .expected
            .keys()
            .chain(self.canonical.keys())
            .chain(self.raw.keys())
            .chain(self.dropped.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        self.exact_one_violations = keys
            .iter()
            .map(|key| {
                let expected = self.expected.get(key).copied().unwrap_or(0);
                let outcomes = self.canonical.get(key).copied().unwrap_or(0)
                    + self.raw.get(key).copied().unwrap_or(0)
                    + self.dropped.get(key).copied().unwrap_or(0);
                if expected == 1 {
                    outcomes.abs_diff(1)
                } else {
                    expected.max(outcomes).max(1)
                }
            })
            .sum();
    }

    fn json(&self) -> Value {
        json!({
            "source_table": self.source_table,
            "logical_class": self.logical_class,
            "source_rows": self.source_rows,
            "canonical_outcomes": self.canonical_outcomes,
            "raw_outcomes": self.raw_outcomes,
            "dropped_outcomes": self.dropped_outcomes,
            "exact_one_violations": self.exact_one_violations,
            "physical_rows": self.physical_rows,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConversionReport {
    pub classes: Vec<ClassAccounting>,
    pub physical_rows: BTreeMap<String, u64>,
}

impl ConversionReport {
    pub(crate) fn verify(&mut self) -> Result<()> {
        for class in &mut self.classes {
            class.verify();
        }
        let violations = self
            .classes
            .iter()
            .map(|class| class.exact_one_violations)
            .sum::<u64>();
        if violations != 0 {
            return Err(ConverterError::Accounting(format!(
                "{violations} logical class contributions lack exact-one outcomes"
            )));
        }
        Ok(())
    }

    pub fn to_pretty_json(&self) -> Result<String> {
        let value = json!({
            "schema_version": 1,
            "source_db": "data/opencrab.db",
            "classes": self.classes.iter().map(ClassAccounting::json).collect::<Vec<_>>(),
            "physical_rows": self.physical_rows,
        });
        let mut rendered = serde_json::to_string_pretty(&value)?;
        rendered.push('\n');
        Ok(rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: u8) -> ContributionKey {
        ContributionKey {
            source_table: "fixture".into(),
            source_key: vec![value],
            row_digest: [value; 32],
        }
    }

    #[test]
    fn keyed_accounting_rejects_duplicate_and_missing_that_cancel_in_totals() {
        let first = key(1);
        let second = key(2);
        let mut accounting = ClassAccounting::new(
            "fixture",
            "fixture_class",
            [first.clone(), second],
            BTreeMap::new(),
        );
        accounting.canonical(first.clone());
        accounting.raw(first);

        accounting.verify();

        assert_eq!(accounting.source_rows, 2);
        assert_eq!(accounting.canonical_outcomes + accounting.raw_outcomes, 2);
        assert_eq!(accounting.exact_one_violations, 2);
    }
}
