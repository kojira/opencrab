use super::*;

// ==================== 観測: dry-run キャプチャ ====================

#[derive(Clone, Debug, Default)]
pub(crate) struct Captured {
    pub(crate) kind: String,
    pub(crate) body: String,
    pub(crate) emoji: String,
    pub(crate) channel: String,
    pub(crate) message: String,
    /// #915: reply の own 投稿 id（dry-run が合成・say の message id と同じ形）。reply 以外は空。
    /// `message`（＝返信先 origin id）は従来どおり維持し、🏁 の相関はこの reply_id で行う。
    pub(crate) reply_id: String,
}

static BUFFER: OnceLock<Arc<Mutex<Vec<Captured>>>> = OnceLock::new();
/// #925 H4: WARN レベルのイベントの `session_id` フィールドだけを溜める。fail-loud の
/// 「発火せず warn」を観測境界にするため（binary 内で共有・累積・session_id で scope する）。
static WARN_BUFFER: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();
static INIT: Once = Once::new();

pub(crate) fn install_capture() -> Arc<Mutex<Vec<Captured>>> {
    let buf = BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    let warn = WARN_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    INIT.call_once(|| {
        let layer = CaptureLayer {
            buf: buf.clone(),
            warn,
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    buf
}

/// WARN イベントのうち指定 `session_id` フィールドを持つものの件数（#925 H4 fail-loud）。
pub(crate) fn warns_with_session(session_id: &str) -> usize {
    WARN_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.as_str() == session_id)
        .count()
}

struct CaptureLayer {
    buf: Arc<Mutex<Vec<Captured>>>,
    warn: Arc<Mutex<Vec<String>>>,
}

#[derive(Default)]
struct WarnVisitor {
    session_id: Option<String>,
}

impl tracing::field::Visit for WarnVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "session_id" {
            self.session_id = Some(value.to_string());
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "session_id" {
            self.session_id = Some(format!("{value:?}"));
        }
    }
}

#[derive(Default)]
struct Visitor {
    kind: Option<String>,
    body: Option<String>,
    emoji: Option<String>,
    channel: Option<String>,
    message: Option<String>,
    reply_id: Option<String>,
}

impl Visitor {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "kind" => self.kind = Some(value),
            "body" => self.body = Some(value),
            "emoji" => self.emoji = Some(value),
            "channel" => self.channel = Some(value),
            "message" => self.message = Some(value),
            "reply_id" => self.reply_id = Some(value),
            _ => {}
        }
    }
}

impl tracing::field::Visit for Visitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set(field.name(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.target() == DRY_RUN_TARGET {
            let mut v = Visitor::default();
            event.record(&mut v);
            self.buf.lock().unwrap().push(Captured {
                kind: v.kind.unwrap_or_default(),
                body: v.body.unwrap_or_default(),
                emoji: v.emoji.unwrap_or_default(),
                channel: v.channel.unwrap_or_default(),
                message: v.message.unwrap_or_default(),
                reply_id: v.reply_id.unwrap_or_default(),
            });
            return;
        }
        // #925 H4: WARN の session_id を溜める（fail-loud の「発火せず warn」の観測境界）。
        if *meta.level() == tracing::Level::WARN {
            let mut wv = WarnVisitor::default();
            event.record(&mut wv);
            if let Some(sid) = wv.session_id {
                self.warn.lock().unwrap().push(sid);
            }
        }
    }
}

pub(crate) fn captured(buf: &Arc<Mutex<Vec<Captured>>>) -> Vec<Captured> {
    buf.lock().unwrap().clone()
}

pub(crate) async fn wait_until(pred: impl Fn() -> bool) -> bool {
    for _ in 0..250 {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred()
}
