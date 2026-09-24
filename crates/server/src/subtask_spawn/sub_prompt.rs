/// サブエンジン用の transport-neutral system prompt。
pub(super) fn sub_system_prompt(
    conn: &rusqlite::Connection,
    agent_id: &str,
    subtask_id: &str,
    depth: u32,
) -> String {
    let (personality, instructions) = opencrab_db::queries::get_agent(conn, agent_id)
        .ok()
        .flatten()
        .map(|a| (a.personality.unwrap_or_default(), a.instructions))
        .unwrap_or_default();
    let personality_section = if personality.is_empty() {
        String::new()
    } else {
        format!("{personality}\n\n")
    };
    let instructions_section = if instructions.is_empty() {
        String::new()
    } else {
        format!("\n\n## Instructions\n{instructions}")
    };
    format!(
        "{personality_section}\
         あなたはサブエンジンとして起動されています。\n\
         - subtask_id: {subtask_id}\n\
         - depth: {depth}\n\
         - 進捗報告は report_progress を使ってください（subtask_id 引数は省略可。省略時はこのサブタスクとして報告されます）\n\
         - 作業予告だけで終了せず、依頼された結果を完成させてください\n\
         - タスク完了時は結果の最終行に NO_REPLY を置いて明示終了してください（NO_REPLY は結果本文から除外されます）\n\
         - 外部への最終配送は親エンジンが行います\n\n\
         You are a sub-engine executing a delegated task.\
         {instructions_section}"
    )
}
