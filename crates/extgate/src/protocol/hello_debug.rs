// D-1091: `Hello` carries the environment its gateway declared for the instance, and those values
// are secrets (account keys). `#[derive(Debug)]` would put them into any future `{:?}`/`?hello` log
// line without anyone editing this type. The manual impl below prints every other field as before
// and reduces the declaration to its names with a fixed placeholder.

use std::collections::BTreeMap;
use std::fmt;

use super::Hello;

impl fmt::Debug for Hello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hello")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("instance_id", &self.instance_id)
            .field("revision", &self.revision)
            .field("config_digest", &self.config_digest)
            .field("operation_protocol", &self.operation_protocol)
            .field("final_delivery", &self.final_delivery)
            .field("operations", &self.operations)
            .field("run_env", &RunEnvNames(&self.run_env))
            .finish()
    }
}

/// The declared names only; every value is replaced by a placeholder before it reaches the
/// formatter, so no code path can print a declared secret.
struct RunEnvNames<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RunEnvNames<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for name in self.0.keys() {
            map.entry(name, &"<redacted>");
        }
        map.finish()
    }
}
