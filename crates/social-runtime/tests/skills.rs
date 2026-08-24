//! ターン context が台帳 `skills` を本体 process.rs と同じ索引形で読む（P2 slice 4）。

use opencrab_engine::*;
use opencrab_port::*;
use opencrab_social_runtime::*;
use opencrab_store::{SkillCreate, Store};
use std::sync::Arc;

struct Harness {
    sys: System,
    eng: ScriptedEngine,
}

const TEST_MODEL: &str = "scripted";

fn build() -> Harness {
    let store = Store::new_in_memory().unwrap();
    store
        .register_model_context_window(TEST_MODEL, 200_000)
        .unwrap();
    let eng = ScriptedEngine::new();
    let host = ScriptedToolHost::new();
    let sys = System::new(
        store,
        Arc::new(eng.clone()),
        Arc::new(host),
        Arc::new(ScriptedShellHost::new()),
        Arc::new(RecordingNotifier::new()),
        Arc::new(CharCounter),
        Config::default(),
    );
    Harness { sys, eng }
}

async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

fn place_pair(
    h: &Harness,
    speaker_standing: Standing,
    speaker_kind: SubjectKind,
) -> (PlaceId, SubjectId, SubjectId) {
    let agent = h
        .sys
        .create_subject(SubjectKind::Agent, "A", "You are A.", Standing::Trusted);
    let speaker = h
        .sys
        .create_subject(speaker_kind, "S", "S", speaker_standing);
    let place = h.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(agent),
        None,
    );
    h.sys.join(place, agent, Role::Participant);
    h.sys.join(place, speaker, Role::Participant);
    (place, agent, speaker)
}

const SKILL_HEADER: &str =
    "Your skills (index only — call read_skill(name) to get a skill's full body):";

fn create_skill(
    store: &Store,
    owner: SubjectId,
    name: &str,
    description: &str,
    visible: bool,
    now: i64,
) {
    store
        .skill_create(
            owner,
            &SkillCreate {
                name: name.into(),
                description: description.into(),
                situation_pattern: "p".into(),
                guidance: "g".into(),
                permission: None,
                visible_to_agent: Some(visible),
            },
            now,
        )
        .unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn owner_turn_sees_all_active_skills() {
    let h = build();
    let (place, agent, owner) = place_pair(&h, Standing::Owner, SubjectKind::Human);
    create_skill(h.sys.store(), agent, "VisibleSkill", "shown", true, 1);
    create_skill(h.sys.store(), agent, "HiddenSkill", "hidden", false, 2);
    h.eng.push(Step::no_reply());
    h.sys.deliver(place, Incoming::said(owner, "hi")).unwrap();
    settle().await;
    let sys = h.eng.last_system().expect("system");
    assert!(sys.contains(SKILL_HEADER), "{sys}");
    assert!(sys.contains("- VisibleSkill: shown"), "{sys}");
    assert!(sys.contains("- HiddenSkill: hidden"), "{sys}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn agent_caller_sees_only_visible_skills() {
    let h = build();
    let (place, agent, peer) = place_pair(&h, Standing::Trusted, SubjectKind::Agent);
    create_skill(h.sys.store(), agent, "VisibleSkill", "shown", true, 1);
    create_skill(h.sys.store(), agent, "HiddenSkill", "hidden", false, 2);
    h.eng.push(Step::no_reply());
    h.sys.deliver(place, Incoming::said(peer, "hi")).unwrap();
    settle().await;
    let sys = h.eng.last_system().expect("system");
    assert!(sys.contains(SKILL_HEADER), "{sys}");
    assert!(sys.contains("- VisibleSkill: shown"), "{sys}");
    assert!(!sys.contains("HiddenSkill"), "{sys}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn empty_skills_omit_section() {
    let h = build();
    let (place, _agent, owner) = place_pair(&h, Standing::Owner, SubjectKind::Human);
    h.eng.push(Step::no_reply());
    h.sys.deliver(place, Incoming::said(owner, "hi")).unwrap();
    settle().await;
    let sys = h.eng.last_system().expect("system");
    assert!(!sys.contains(SKILL_HEADER), "{sys}");
}
