//! 段階移行フラグ。既定は旧 in-process ループ。旧経路は削除しない。

/// `legacy`（既定） / `v3_shadow` / `v3`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NostrIngress {
    #[default]
    Legacy,
    V3Shadow,
    V3,
}

impl NostrIngress {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "" | "legacy" => Some(Self::Legacy),
            "v3_shadow" => Some(Self::V3Shadow),
            "v3" => Some(Self::V3),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::V3Shadow => "v3_shadow",
            Self::V3 => "v3",
        }
    }

    /// 旧 in-process default/watch ループを回す。
    pub fn runs_legacy_loops(self) -> bool {
        matches!(self, Self::Legacy | Self::V3Shadow)
    }

    /// instance/binding を DB に敷設する。
    pub fn provisions_binding(self) -> bool {
        matches!(self, Self::V3)
    }

    /// Binding PUT / said / say を行わない shadow。
    pub fn shadows_only(self) -> bool {
        matches!(self, Self::V3Shadow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_legacy() {
        assert_eq!(NostrIngress::default(), NostrIngress::Legacy);
        assert_eq!(NostrIngress::parse(""), Some(NostrIngress::Legacy));
        assert_eq!(NostrIngress::parse("legacy"), Some(NostrIngress::Legacy));
        assert!(NostrIngress::Legacy.runs_legacy_loops());
        assert!(!NostrIngress::Legacy.provisions_binding());
        assert!(!NostrIngress::Legacy.shadows_only());
    }

    #[test]
    fn v3_stops_legacy_loops_and_provisions() {
        let m = NostrIngress::parse("v3").unwrap();
        assert_eq!(m, NostrIngress::V3);
        assert!(!m.runs_legacy_loops());
        assert!(m.provisions_binding());
        assert!(!m.shadows_only());
    }

    #[test]
    fn v3_shadow_keeps_legacy_and_skips_binding_put() {
        let m = NostrIngress::parse("v3_shadow").unwrap();
        assert!(m.runs_legacy_loops());
        assert!(!m.provisions_binding());
        assert!(m.shadows_only());
    }

    #[test]
    fn unknown_is_none() {
        assert!(NostrIngress::parse("banana").is_none());
        assert!(NostrIngress::parse("V3").is_none());
    }
}
