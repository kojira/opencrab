//! Nostr ingress mode. The external V3 gateway is the only supported runtime.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NostrIngress {
    #[default]
    V3,
}

impl NostrIngress {
    pub fn parse(raw: &str) -> Option<Self> {
        (raw.trim() == "v3").then_some(Self::V3)
    }

    pub fn as_str(self) -> &'static str {
        "v3"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_v3_is_accepted() {
        assert_eq!(NostrIngress::parse("v3"), Some(NostrIngress::V3));
        for removed in ["", "legacy", "v3_shadow", "V3", "banana"] {
            assert_eq!(NostrIngress::parse(removed), None, "{removed}");
        }
    }
}
