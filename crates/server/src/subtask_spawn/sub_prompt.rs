/// サブエンジン用の system prompt（旧 Discord 実装の文面をそのまま保つ）。
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
         - Discordへの直接送信は禁止されています\n\
         - 進捗報告は report_progress を使ってください（subtask_id 引数は省略可。省略時はこのサブタスクとして報告されます）\n\
         - タスク完了時はテキストで結果を返してください（Discord送信はメインエンジンが行います）\n\n\
         You are a sub-engine executing a delegated task.\
         {instructions_section}"
    )
}
