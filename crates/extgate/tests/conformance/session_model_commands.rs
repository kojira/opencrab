// D-1080: session model commands target the authenticated binding's canonical session.

impl TestRuntime {
    fn write_session_model(
        &self,
        session_id: &str,
        profile: TurnModelProfile,
        model: Option<&str>,
    ) -> Result<(), ModelAdminError> {
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let written = match profile {
            TurnModelProfile::Default => {
                opencrab_db::queries::set_session_model(&conn, session_id, model)
            }
            TurnModelProfile::Realtime => {
                opencrab_db::queries::set_session_realtime_model(&conn, session_id, model)
            }
        };
        match written {
            Ok(true) => Ok(()),
            Ok(false) => Err(ModelAdminError::TargetUnavailable),
            Err(_) => Err(ModelAdminError::Internal),
        }
    }

    async fn session_model_list(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<SessionModelList, ModelAdminError> {
        let default_model = self.list_models(agent_id).await?.current_model;
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let configured_model = opencrab_db::queries::get_session_model(&conn, session_id)
            .map_err(|_| ModelAdminError::TargetUnavailable)?;
        let realtime_configured_model =
            opencrab_db::queries::get_session_realtime_model(&conn, session_id)
                .map_err(|_| ModelAdminError::TargetUnavailable)?;
        let current_model = configured_model.clone().unwrap_or(default_model.clone());
        Ok(SessionModelList {
            realtime_current_model: realtime_configured_model
                .clone()
                .unwrap_or(current_model.clone()),
            realtime_configured_model,
            snapshot: ModelSnapshot {
                models: vec!["openai:gpt-5".to_string()],
                current_model,
                configured_model,
                default_model,
            },
        })
    }

    fn effort_slot(profile: TurnModelProfile) -> opencrab_db::queries::SessionEffortSlot {
        match profile {
            TurnModelProfile::Default => opencrab_db::queries::SessionEffortSlot::Default,
            TurnModelProfile::Realtime => opencrab_db::queries::SessionEffortSlot::Realtime,
        }
    }

    fn write_effort(
        &self,
        session_id: &str,
        profile: TurnModelProfile,
        effort: Option<&str>,
    ) -> Result<(), ModelAdminError> {
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        match opencrab_db::queries::set_session_reasoning_effort(
            &conn,
            session_id,
            Self::effort_slot(profile),
            effort,
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err(ModelAdminError::TargetUnavailable),
            Err(_) => Err(ModelAdminError::Internal),
        }
    }

    fn effort_snapshot(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> Result<EffortSnapshot, ModelAdminError> {
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let slot = Self::effort_slot(profile);
        let configured_effort =
            opencrab_db::queries::get_session_reasoning_effort(&conn, session_id, slot)
                .map_err(|_| ModelAdminError::TargetUnavailable)?;
        let (current_effort, source) =
            opencrab_db::queries::effective_reasoning_effort_for_session(
                &conn, agent_id, session_id, slot,
            )
            .map_err(|_| ModelAdminError::Internal)?;
        Ok(EffortSnapshot {
            configured_effort,
            current_effort,
            source,
        })
    }

    async fn session_model_snapshot(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        let list = self.session_model_list(agent_id, session_id).await?;
        Ok(match profile {
            TurnModelProfile::Default => list.snapshot,
            TurnModelProfile::Realtime => ModelSnapshot {
                models: list.snapshot.models,
                configured_model: list.realtime_configured_model,
                current_model: list.realtime_current_model,
                default_model: list.snapshot.current_model,
            },
        })
    }
}

#[async_trait]
impl ModelAdministration for TestRuntime {
    async fn list_models(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
        let configured_model = self
            .db
            .lock()
            .map_err(|_| ModelAdminError::Internal)?
            .query_row(
                "SELECT model FROM agents WHERE agent_id = ?1",
                [agent_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|_| ModelAdminError::Internal)?;
        Ok(ModelSnapshot {
            models: vec!["openai:gpt-5".to_string()],
            current_model: configured_model
                .clone()
                .unwrap_or_else(|| "mock:test".to_string()),
            configured_model,
            default_model: "mock:test".to_string(),
        })
    }

    async fn set_model(
        &self,
        agent_id: &str,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        if model != "openai:gpt-5" && model != "gpt-5" {
            return Err(ModelAdminError::NotFound);
        }
        self.db
            .lock()
            .map_err(|_| ModelAdminError::Internal)?
            .execute(
                "UPDATE agents SET model = 'openai:gpt-5' WHERE agent_id = ?1",
                [agent_id],
            )
            .map_err(|_| ModelAdminError::Internal)?;
        self.list_models(agent_id).await
    }

    async fn reset_model(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
        self.db
            .lock()
            .map_err(|_| ModelAdminError::Internal)?
            .execute("UPDATE agents SET model = NULL WHERE agent_id = ?1", [agent_id])
            .map_err(|_| ModelAdminError::Internal)?;
        self.list_models(agent_id).await
    }
    async fn list_session_models(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<SessionModelList, ModelAdminError> {
        self.session_model_list(agent_id, session_id).await
    }

    async fn set_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        if model != "openai:gpt-5" && model != "gpt-5" {
            return Err(ModelAdminError::NotFound);
        }
        self.write_session_model(session_id, profile, Some("openai:gpt-5"))?;
        self.session_model_snapshot(agent_id, session_id, profile).await
    }

    async fn reset_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        self.write_session_model(session_id, profile, None)?;
        self.session_model_snapshot(agent_id, session_id, profile).await
    }
    async fn list_session_reasoning_efforts(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<SessionEffortList, ModelAdminError> {
        Ok(SessionEffortList {
            default: self.effort_snapshot(agent_id, session_id, TurnModelProfile::Default)?,
            realtime: self.effort_snapshot(agent_id, session_id, TurnModelProfile::Realtime)?,
        })
    }

    async fn set_session_reasoning_effort(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
        effort: &str,
    ) -> Result<EffortSnapshot, ModelAdminError> {
        if !REASONING_EFFORTS.contains(&effort) {
            return Err(ModelAdminError::InvalidArgs);
        }
        self.write_effort(session_id, profile, Some(effort))?;
        self.effort_snapshot(agent_id, session_id, profile)
    }

    async fn reset_session_reasoning_effort(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> Result<EffortSnapshot, ModelAdminError> {
        self.write_effort(session_id, profile, None)?;
        self.effort_snapshot(agent_id, session_id, profile)
    }
}

fn stored_session_model(h: &Harness, session_id: &str) -> Option<String> {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::get_session_model(&conn, session_id).unwrap()
}

fn stored_agent_model(h: &Harness) -> Option<String> {
    h.state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT model FROM agents WHERE agent_id = 'agent-1'", [], |row| row.get(0))
        .unwrap()
}

async fn send_command(
    s: &mut UnixStream,
    id: &str,
    binding_id: &str,
    caller: Value,
    name: &str,
    args: Value,
) -> Value {
    write_frame(
        s,
        &json!({
            "m": "command",
            "id": id,
            "binding_id": binding_id,
            "caller": caller,
            "name": name,
            "args": args,
        }),
    )
    .await;
    read_until(s, |v| v["id"] == id).await
}

#[tokio::test]
async fn session_model_commands_change_only_the_bound_session() {
    let h = Harness::start().await;
    insert_named_session(&h, "other-session");
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);

    let set = send_command(
        &mut s,
        "cmd-set",
        &binding_id,
        json!({"role": "owner"}),
        "set_session_model",
        json!({"model": "gpt-5"}),
    )
    .await;
    assert_eq!(set["m"], "ok", "{set}");
    assert_eq!(set["result"]["scope"], "session");
    assert_eq!(set["result"]["configured_model"], "openai:gpt-5");
    assert_eq!(set["result"]["current_model"], "openai:gpt-5");
    assert_eq!(set["result"]["applies"], "next_turn");
    assert_eq!(stored_session_model(&h, &session_id).as_deref(), Some("openai:gpt-5"));
    assert_eq!(stored_session_model(&h, "other-session"), None);
    assert_eq!(stored_agent_model(&h), None);

    let list = send_command(
        &mut s,
        "cmd-list",
        &binding_id,
        json!({"role": "owner"}),
        "list_session_models",
        json!({}),
    )
    .await;
    assert_eq!(list["m"], "ok", "{list}");
    assert_eq!(list["result"]["configured_model"], "openai:gpt-5");

    // A client-supplied session target is not an accepted argument.
    let spoof = send_command(
        &mut s,
        "cmd-spoof",
        &binding_id,
        json!({"role": "owner"}),
        "set_session_model",
        json!({"model": "gpt-5", "session_id": "other-session"}),
    )
    .await;
    assert_eq!(spoof["code"], "invalid_args");
    assert_eq!(stored_session_model(&h, "other-session"), None);

    let reset = send_command(
        &mut s,
        "cmd-reset",
        &binding_id,
        json!({"role": "owner"}),
        "reset_session_model",
        json!({}),
    )
    .await;
    assert_eq!(reset["m"], "ok", "{reset}");
    assert_eq!(reset["result"]["configured_model"], Value::Null);
    assert_eq!(stored_session_model(&h, &session_id), None);
    assert_eq!(stored_agent_model(&h), None);
}

#[tokio::test]
async fn session_model_commands_reject_non_owner_unknown_and_closed_bindings() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);

    let co_agent = send_command(
        &mut s,
        "cmd-co",
        &binding_id,
        json!({"role": "co_agent", "agent_id": "agent-b"}),
        "set_session_model",
        json!({"model": "gpt-5"}),
    )
    .await;
    assert_eq!(co_agent["code"], "forbidden");

    let unknown = send_command(
        &mut s,
        "cmd-unknown",
        &uuid(),
        json!({"role": "owner"}),
        "set_session_model",
        json!({"model": "gpt-5"}),
    )
    .await;
    assert_eq!(unknown["code"], "binding_not_found");

    h.state
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE gate_bindings SET closed_at = 1 WHERE binding_id = ?1",
            [&binding_id],
        )
        .unwrap();
    let closed = send_command(
        &mut s,
        "cmd-closed",
        &binding_id,
        json!({"role": "owner"}),
        "set_session_model",
        json!({"model": "gpt-5"}),
    )
    .await;
    assert_eq!(closed["code"], "binding_not_found");
    assert_eq!(stored_session_model(&h, &session_id), None);
    assert_eq!(stored_agent_model(&h), None);
}

/// A command during a held turn is answered at once and the connection keeps reading.
#[tokio::test(start_paused = true)]
async fn session_model_set_does_not_wait_for_the_running_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    let entered = h.runtime.turn_entered.notified();
    tokio::pin!(entered);

    write_frame(
        &mut s,
        &json!({
            "id": "s-1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-1",
            "author_id": "u",
            "text": "first",
            "attachments": []
        }),
    )
    .await;
    for _ in 0..80 {
        tokio::select! {
            _ = &mut entered => break,
            _ = tokio::time::advance(Duration::from_millis(5)) => {}
        }
    }
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0, "turn is held");

    let set = send_command(
        &mut s,
        "cmd-set-held",
        &binding_id,
        json!({"role": "owner"}),
        "set_session_model",
        json!({"model": "gpt-5"}),
    )
    .await;
    assert_eq!(set["m"], "ok", "{set}");
    assert_eq!(stored_session_model(&h, &session_id).as_deref(), Some("openai:gpt-5"));

    write_frame(
        &mut s,
        &json!({
            "id": "s-2",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-2",
            "author_id": "u",
            "text": "second",
            "attachments": []
        }),
    )
    .await;
    let ok = read_said_response(&mut s, "s-2").await;
    assert_eq!(ok["m"], "ok", "later message on the same connection is processed");
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0, "turn still held");
    release_tx.send(()).unwrap();
}

fn stored_realtime_model(h: &Harness, session_id: &str) -> Option<String> {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::get_session_realtime_model(&conn, session_id).unwrap()
}

/// D-1081: `profile:"realtime"` targets only the realtime slot of the bound session, echoes the
/// profile, and any other argument shape is `invalid_args` with nothing written.
#[tokio::test]
async fn realtime_profile_commands_change_only_the_realtime_slot() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    let owner = || json!({"role": "owner"});

    let set = send_command(
        &mut s,
        "rt-set",
        &binding_id,
        owner(),
        "set_session_model",
        json!({"model": "gpt-5", "profile": "realtime"}),
    )
    .await;
    assert_eq!(set["m"], "ok", "{set}");
    assert_eq!(set["result"]["profile"], "realtime");
    assert_eq!(set["result"]["configured_model"], "openai:gpt-5");
    assert_eq!(set["result"]["current_model"], "openai:gpt-5");
    assert_eq!(set["result"]["default_model"], "mock:test");
    assert_eq!(set["result"]["applies"], "next_turn");
    assert_eq!(stored_realtime_model(&h, &session_id).as_deref(), Some("openai:gpt-5"));
    assert_eq!(stored_session_model(&h, &session_id), None);

    let list = send_command(&mut s, "rt-list", &binding_id, owner(), "list_session_models", json!({})).await;
    assert_eq!(list["m"], "ok", "{list}");
    assert_eq!(list["result"]["configured_model"], Value::Null);
    assert_eq!(list["result"]["current_model"], "mock:test");
    assert_eq!(list["result"]["realtime_configured_model"], "openai:gpt-5");
    assert_eq!(list["result"]["realtime_current_model"], "openai:gpt-5");

    for (id, name, args) in [
        ("bad-1", "set_session_model", json!({"model": "gpt-5", "profile": "default"})),
        ("bad-2", "set_session_model", json!({"model": "gpt-5", "profile": "bogus"})),
        ("bad-3", "set_session_model", json!({"model": "gpt-5", "profile": true})),
        ("bad-4", "set_session_model", json!({"profile": "realtime"})),
        ("bad-5", "set_session_model", json!({"model": "gpt-5", "profile": "realtime", "x": 1})),
        ("bad-6", "reset_session_model", json!({"profile": "default"})),
        ("bad-7", "reset_session_model", json!({"profile": "realtime", "x": 1})),
        ("bad-8", "reset_session_model", json!({"model": "gpt-5"})),
        ("bad-9", "list_session_models", json!({"profile": "realtime"})),
    ] {
        let reply = send_command(&mut s, id, &binding_id, owner(), name, args).await;
        assert_eq!(reply["code"], "invalid_args", "{id}: {reply}");
    }
    assert_eq!(stored_realtime_model(&h, &session_id).as_deref(), Some("openai:gpt-5"));
    assert_eq!(stored_session_model(&h, &session_id), None);

    let co_agent = send_command(
        &mut s,
        "rt-co",
        &binding_id,
        json!({"role": "co_agent", "agent_id": "agent-b"}),
        "reset_session_model",
        json!({"profile": "realtime"}),
    )
    .await;
    assert_eq!(co_agent["code"], "forbidden");

    let reset = send_command(
        &mut s,
        "rt-reset",
        &binding_id,
        owner(),
        "reset_session_model",
        json!({"profile": "realtime"}),
    )
    .await;
    assert_eq!(reset["m"], "ok", "{reset}");
    assert_eq!(reset["result"]["profile"], "realtime");
    assert_eq!(reset["result"]["configured_model"], Value::Null);
    assert_eq!(reset["result"]["current_model"], "mock:test");
    assert_eq!(reset["result"]["default_model"], "mock:test");
    assert_eq!(stored_realtime_model(&h, &session_id), None);
    assert_eq!(stored_agent_model(&h), None);
}

fn stored_effort(
    h: &Harness,
    session_id: &str,
    slot: opencrab_db::queries::SessionEffortSlot,
) -> Option<String> {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::get_session_reasoning_effort(&conn, session_id, slot).unwrap()
}

/// D-1086: the session reasoning effort commands target the bound session's slots: set and reset
/// echo the slot's effective value and source, list shows both slots, a value outside
/// none/low/medium/high or any other argument shape is `invalid_args`, and a non-owner is
/// `forbidden`, with nothing written.
#[tokio::test]
async fn session_reasoning_effort_commands_change_only_the_bound_session_slot() {
    use opencrab_db::queries::SessionEffortSlot::{Default as DefaultSlot, Realtime as RealtimeSlot};
    let h = Harness::start().await;
    insert_named_session(&h, "other-session");
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    let owner = || json!({"role": "owner"});

    let list = send_command(&mut s, "e-list0", &binding_id, owner(), "list_session_reasoning_efforts", json!({})).await;
    assert_eq!(list["m"], "ok", "{list}");
    assert_eq!(list["result"]["scope"], "session");
    assert_eq!(list["result"]["efforts"], json!(["none", "low", "medium", "high"]));
    assert_eq!(list["result"]["configured_effort"], Value::Null);
    assert_eq!(list["result"]["current_effort"], Value::Null);
    assert_eq!(list["result"]["source"], "provider");
    assert_eq!(list["result"]["realtime_configured_effort"], Value::Null);
    assert_eq!(list["result"]["realtime_source"], "provider");

    let set = send_command(&mut s, "e-set", &binding_id, owner(), "set_session_reasoning_effort", json!({"effort": "medium"})).await;
    assert_eq!(set["m"], "ok", "{set}");
    assert_eq!(set["result"]["scope"], "session");
    assert_eq!(set["result"]["configured_effort"], "medium");
    assert_eq!(set["result"]["current_effort"], "medium");
    assert_eq!(set["result"]["source"], "session");
    assert_eq!(set["result"]["applies"], "next_turn");
    assert!(set["result"].get("profile").is_none());

    let rt = send_command(&mut s, "e-rt", &binding_id, owner(), "set_session_reasoning_effort", json!({"effort": "none", "profile": "realtime"})).await;
    assert_eq!(rt["m"], "ok", "{rt}");
    assert_eq!(rt["result"]["profile"], "realtime");
    assert_eq!(rt["result"]["current_effort"], "none");
    assert_eq!(stored_effort(&h, &session_id, DefaultSlot).as_deref(), Some("medium"));
    assert_eq!(stored_effort(&h, &session_id, RealtimeSlot).as_deref(), Some("none"));
    assert_eq!(stored_effort(&h, "other-session", DefaultSlot), None);

    let list = send_command(&mut s, "e-list1", &binding_id, owner(), "list_session_reasoning_efforts", json!({})).await;
    assert_eq!(list["result"]["current_effort"], "medium");
    assert_eq!(list["result"]["realtime_current_effort"], "none");
    assert_eq!(list["result"]["realtime_source"], "session");

    for (id, name, args) in [
        ("eb-1", "set_session_reasoning_effort", json!({"effort": "minimal"})),
        ("eb-2", "set_session_reasoning_effort", json!({"effort": "HIGH"})),
        ("eb-3", "set_session_reasoning_effort", json!({"effort": 1})),
        ("eb-4", "set_session_reasoning_effort", json!({})),
        ("eb-5", "set_session_reasoning_effort", json!({"effort": "low", "profile": "default"})),
        ("eb-6", "set_session_reasoning_effort", json!({"effort": "low", "session_id": "other-session"})),
        ("eb-7", "reset_session_reasoning_effort", json!({"effort": "low"})),
        ("eb-8", "list_session_reasoning_efforts", json!({"profile": "realtime"})),
    ] {
        let reply = send_command(&mut s, id, &binding_id, owner(), name, args).await;
        assert_eq!(reply["code"], "invalid_args", "{id}: {reply}");
    }
    let co_agent = send_command(
        &mut s,
        "e-co",
        &binding_id,
        json!({"role": "co_agent", "agent_id": "agent-b"}),
        "set_session_reasoning_effort",
        json!({"effort": "high"}),
    )
    .await;
    assert_eq!(co_agent["code"], "forbidden");
    assert_eq!(stored_effort(&h, &session_id, DefaultSlot).as_deref(), Some("medium"));
    assert_eq!(stored_effort(&h, &session_id, RealtimeSlot).as_deref(), Some("none"));

    let reset = send_command(&mut s, "e-reset", &binding_id, owner(), "reset_session_reasoning_effort", json!({"profile": "realtime"})).await;
    assert_eq!(reset["m"], "ok", "{reset}");
    assert_eq!(reset["result"]["profile"], "realtime");
    assert_eq!(reset["result"]["configured_effort"], Value::Null);
    assert_eq!(reset["result"]["source"], "provider");
    assert_eq!(stored_effort(&h, &session_id, RealtimeSlot), None);
    assert_eq!(stored_effort(&h, &session_id, DefaultSlot).as_deref(), Some("medium"));
}
