use std::collections::HashSet;

/// 1 engine execution内だけで、exact requestへ取り込んだopaque originの結果を追跡する。
/// DB・設定・platform解釈を持たず、初回request取り込み順を維持する。
pub(super) struct SilentOriginTracker {
    read: HashSet<String>,
    awaiting: Vec<String>,
    silent: Vec<String>,
}

impl SilentOriginTracker {
    pub(super) fn new() -> Self {
        Self {
            read: HashSet::new(),
            awaiting: Vec::new(),
            silent: Vec::new(),
        }
    }

    pub(super) fn was_read(&self, origin: &str) -> bool {
        self.read.contains(origin)
    }

    /// request呼出し直前に、そのrequestへ初めて入るoriginをoutcome待ちへ移す。
    pub(super) fn include_in_request(&mut self, origin: String) -> bool {
        if !self.read.insert(origin.clone()) {
            return false;
        }
        self.awaiting.push(origin);
        true
    }

    /// 成功した可視speech／utteranceは、そのrequestまでの未解決originを可視解決する。
    pub(super) fn resolve_visible(&mut self) {
        self.awaiting.clear();
    }

    /// visible textのない明示NO_REPLYだけが未解決originをsilentへ確定する。
    pub(super) fn resolve_silent(&mut self) {
        for origin in self.awaiting.drain(..) {
            if !self.silent.contains(&origin) {
                self.silent.push(origin);
            }
        }
    }

    pub(super) fn into_silent(self) -> Vec<String> {
        self.silent
    }
}
