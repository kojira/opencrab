use crate::{ConverterError, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassAccounting {
    pub source_table: String,
    pub logical_class: String,
    pub source_rows: u64,
    pub canonical_outcomes: u64,
    pub raw_outcomes: u64,
    pub exact_one_violations: u64,
    pub physical_rows: BTreeMap<String, u64>,
}

impl ClassAccounting {
    pub(crate) fn verify(&mut self) {
        self.exact_one_violations = self
            .source_rows
            .abs_diff(self.canonical_outcomes + self.raw_outcomes);
    }

    fn json(&self) -> Value {
        json!({
            "source_table": self.source_table,
            "logical_class": self.logical_class,
            "source_rows": self.source_rows,
            "canonical_outcomes": self.canonical_outcomes,
            "raw_outcomes": self.raw_outcomes,
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
