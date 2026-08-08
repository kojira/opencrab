//! テスト専用のヘルパ。
//!
//! fake nostaro（テストが書き出して spawn する小さなシェルスクリプト）の materialize を
//! 一箇所に集める。

use std::path::{Path, PathBuf};

/// `dir` に実行可能な fake nostaro スクリプトを置き、そのパスを返す。
///
/// **中身を書いたプロセス自身が exec すると ETXTBSY で散発的に落ちる**（#427）。
/// `std::fs::write` は戻る時点で fd を閉じているが、テストバイナリは多スレッドで
/// 並行に子プロセスを spawn しており、fork〜exec の窓で兄弟スレッドの子が書き込み fd を
/// 引き継ぐ。その子が exec する（＝O_CLOEXEC で閉じる）までの間に本体が execve すると
/// カーネルは「書き込み中の実行ファイル」と見なして ETXTBSY を返す。
///
/// そこで**本体は exec 対象の inode を書き込みで開かない**。中身は別名の下書きへ書き、
/// exec 対象は子プロセス（`cp`）に作らせる。書き込み fd は `cp` のアドレス空間にしか
/// 存在せず、本体の fork では複製されない。`cp` の終了を待ってから返すので、exec 時点で
/// その inode を書き込みで開いているプロセスはどこにも居ない。
pub(crate) fn write_fake_nostaro(dir: &Path, body: &str) -> PathBuf {
    let script = dir.join("fake-nostaro.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let draft = dir.join("fake-nostaro.draft");
        std::fs::write(&draft, body).unwrap();
        let status = std::process::Command::new("cp")
            .arg(&draft)
            .arg(&script)
            .status()
            .expect("cp で fake nostaro を materialize できること");
        assert!(status.success(), "cp が失敗した: {status:?}");
        // chmod はファイルを書き込みで開かないので ETXTBSY の窓を作らない。
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&script, body).unwrap();
    }
    script
}
