//! 本番の shell 実行（core builtin `core-shell` の seam 実装・DESIGN-shell.md）。
//!
//! core は `argv`（実行ファイル＋引数の**構造化配列**）と `cwd`（subject ごとの作業領域の相対トークン）を
//! 渡す。ここは **`argv[0]` を実行ファイル、残りを引数として直接 exec** する（`sh -c` を経由しない
//! ＝シェルは介さず `;`・`|`・`>` 等は解釈されない＝注入不可）。切り離し・退避・停止・上限は core の
//! 既存の背景の機構が担うので、この実装は「走らせて結果を返す」だけ。
//!
//! 停止（core-bg-stop）・上限（bg_cap）は core がタスクを `abort` する形で効く。`abort` でこの future が
//! drop されると子プロセスも道連れに殺す必要があるので **`kill_on_drop(true)`** を必ず立てる（cursor.rs と
//! 同じ流儀・孤児プロセスを残さない）。

use async_trait::async_trait;
use opencrab_port::{ShellHost, ToolError};
use std::path::PathBuf;
use tokio::process::Command;

/// tokio::process で直接 exec する ShellHost。`root` の下に subject ごとの作業領域を作って走らせる。
///
/// `root` は deployment の設定（環境変数 `OPENCRAB_SHELL_ROOT`）。shell は既定で off（subject_allowed_tools が
/// 空）なので、shell を使わない構成では `root=None` のままで問題ない——**使われたときにだけ** fail loud
/// する（既定パスを発明して黙って別の場所で走らせない・§15）。
pub struct TokioShellHost {
    root: Option<PathBuf>,
}

impl TokioShellHost {
    pub fn new(root: Option<PathBuf>) -> TokioShellHost {
        TokioShellHost { root }
    }
}

#[async_trait]
impl ShellHost for TokioShellHost {
    async fn run(&self, argv: &[String], cwd: &str) -> Result<String, ToolError> {
        // root 未設定で shell が使われた → fail loud（黙って既定の場所で走らせない）。
        let root = self.root.as_ref().ok_or_else(|| {
            ToolError(
                "OPENCRAB_SHELL_ROOT が未設定: shell の作業領域が無い（shell を使うなら設定する）"
                    .into(),
            )
        })?;
        // argv は core（shell_argv_from_args）で空でないことが保証済みだが、seam の契約として自衛する。
        let program = argv
            .first()
            .ok_or_else(|| ToolError("argv が空（実行ファイルが無い）".into()))?;
        let workdir = root.join(cwd);
        tokio::fs::create_dir_all(&workdir)
            .await
            .map_err(|e| ToolError(format!("作業領域を作れない（{}）: {e}", workdir.display())))?;

        let mut cmd = Command::new(program);
        cmd.args(&argv[1..]);
        cmd.current_dir(&workdir);
        // 上限・停止でタスクが abort されたら子プロセスも殺す（孤児を残さない）。
        cmd.kill_on_drop(true);

        let out = cmd
            .output()
            .await
            .map_err(|e| ToolError(format!("実行できない（{program}）: {e}")))?;

        // stdout に続けて stderr を素のテキストで返す（大きければ core が退避する・生 JSON にしない）。
        let mut body = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.stderr.is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        // 非0終了は「失敗した結果」として Err で返す（決着が成功/失敗を正しく出す・§15）。出力は本文に残す。
        if out.status.success() {
            Ok(body)
        } else {
            let code = out
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            Err(ToolError(format!("exit {code}:\n{body}")))
        }
    }
}
