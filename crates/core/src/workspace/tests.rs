#[test]
fn resolve_agent_workspace_expands_and_validates() {
    let p = super::resolve_agent_workspace("data/agents/{agent_id}/workspace", "crab-1").unwrap();
    assert_eq!(p, std::path::PathBuf::from("data/agents/crab-1/workspace"));

    // 検証を必ず通す: トラバーサル/空は拒否
    assert!(super::resolve_agent_workspace("data/{agent_id}", "../evil").is_err());
    assert!(super::resolve_agent_workspace("data/{agent_id}", "a/b").is_err());
    assert!(super::resolve_agent_workspace("data/{agent_id}", "").is_err());

    // テンプレートに {agent_id} が無い場合は素通し（共有ベース運用）
    let p = super::resolve_agent_workspace("/tmp", "crab-1").unwrap();
    assert_eq!(p, std::path::PathBuf::from("/tmp"));
}

use super::*;

fn temp_workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::TempDir::new().unwrap();
    let ws = Workspace::from_root(dir.path()).unwrap();
    (dir, ws)
}

#[test]
fn test_new() {
    let dir = tempfile::TempDir::new().unwrap();
    let ws = Workspace::new("agent-1", dir.path().to_str().unwrap()).unwrap();
    let expected = dir.path().join("workspaces").join("agent-1");
    assert!(expected.exists());
    assert!(ws.root().ends_with("workspaces/agent-1"));
}

#[test]
fn test_from_root() {
    let dir = tempfile::TempDir::new().unwrap();
    let ws = Workspace::from_root(dir.path()).unwrap();
    assert!(ws.root().exists());
    assert_eq!(ws.root(), dir.path().canonicalize().unwrap());
}

#[test]
fn test_write_and_read() {
    let (_dir, ws) = temp_workspace();
    ws.write_file("test.txt", "hello").unwrap();
    let content = ws.read_file("test.txt").unwrap();
    assert_eq!(content, "hello");
}

#[test]
fn test_parent_auto_create() {
    let (_dir, ws) = temp_workspace();
    ws.write_file("a/b/c.txt", "x").unwrap();
    let content = ws.read_file("a/b/c.txt").unwrap();
    assert_eq!(content, "x");
}

#[test]
fn test_edit() {
    let (_dir, ws) = temp_workspace();
    ws.write_file("f.txt", "hello old world").unwrap();
    let count = ws.edit_file("f.txt", "old", "new").unwrap();
    assert_eq!(count, 1);
    let content = ws.read_file("f.txt").unwrap();
    assert_eq!(content, "hello new world");
}

#[test]
fn test_list() {
    let (_dir, ws) = temp_workspace();
    ws.write_file("aaa.txt", "a").unwrap();
    ws.write_file("bbb.txt", "b").unwrap();
    let entries = ws.list_dir("").unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "aaa.txt");
    assert_eq!(entries[1].name, "bbb.txt");
}

#[test]
fn test_delete() {
    let (_dir, ws) = temp_workspace();
    ws.write_file("del.txt", "bye").unwrap();
    ws.delete_file("del.txt").unwrap();
    assert!(ws.read_file("del.txt").is_err());
}

#[test]
fn test_mkdir() {
    let (_dir, ws) = temp_workspace();
    ws.mkdir_sync("newdir").unwrap();
    let entries = ws.list_dir("").unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_dir);
    assert_eq!(entries[0].name, "newdir");
}

#[test]
fn test_traversal_dotdot() {
    let (_dir, ws) = temp_workspace();
    assert!(ws.resolve_path("../escape").is_err());
}

#[test]
fn test_absolute_path() {
    let (_dir, ws) = temp_workspace();
    assert!(ws.resolve_path("/etc/passwd").is_err());
}

#[test]
fn test_complex_traversal() {
    let (_dir, ws) = temp_workspace();
    assert!(ws.resolve_path("a/../../escape").is_err());
}

#[test]
fn test_safe_dot_path() {
    let (_dir, ws) = temp_workspace();
    let result = ws.resolve_path("./valid.txt");
    assert!(result.is_ok());
}

#[test]
fn test_empty_path() {
    let (_dir, ws) = temp_workspace();
    let result = ws.resolve_path("").unwrap();
    assert_eq!(result, ws.root());
}

#[test]
fn test_delete_dir_fails() {
    let (_dir, ws) = temp_workspace();
    ws.mkdir_sync("dir").unwrap();
    assert!(ws.delete_file("dir").is_err());
}

/// #617: `line_reader` は **1 回の open** で `next_line` を繰り返して全行を順に返す（毎行
/// open+seek し直す O(n²) ではない）。単一 open であることは、1 つの reader インスタンスから
/// 連続した行番号・本文が順に出てくることで観測できる。
#[test]
fn line_reader_reads_sequentially_from_single_open() {
    let (_dir, ws) = temp_workspace();
    ws.write_file("multi.txt", "alpha\nbravo\ncharlie\ndelta")
        .unwrap();

    // start_line=2 から。open は 1 回だけ、以降は同じ reader を前進させる。
    let (mut reader, total) = ws.line_reader("multi.txt", 2, 512).unwrap();
    assert_eq!(total, "alpha\nbravo\ncharlie\ndelta".len() as u64);

    let l2 = reader.next_line().unwrap().unwrap();
    assert_eq!(
        (l2.number, l2.text.as_str(), l2.overflow_chars),
        (2, "bravo", 0)
    );
    let l3 = reader.next_line().unwrap().unwrap();
    assert_eq!(l3.number, 3);
    assert_eq!(l3.text, "charlie");
    let l4 = reader.next_line().unwrap().unwrap();
    assert_eq!((l4.number, l4.text.as_str()), (4, "delta")); // 末尾に改行なし
    assert!(reader.next_line().unwrap().is_none(), "EOF");
}

/// 1 行あたりの文字数上限が効き、超過ぶんは `overflow_chars` に出る（バイトではなく文字数）。
/// マルチバイト（3 バイト/字）でも切りは文字境界で、割れた文字は出さない。
#[test]
fn line_reader_truncates_by_chars_not_bytes() {
    let (_dir, ws) = temp_workspace();
    // "あ"×10（30 バイト）を 1 行。max_chars=4 で 4 文字だけ、6 文字あふれる。
    ws.write_file("jp.txt", &"あ".repeat(10)).unwrap();
    let (mut reader, _total) = ws.line_reader("jp.txt", 1, 4).unwrap();
    let l = reader.next_line().unwrap().unwrap();
    assert_eq!(l.text, "ああああ", "文字境界で 4 文字だけ返す");
    assert_eq!(l.text.chars().count(), 4);
    assert_eq!(l.overflow_chars, 6, "切り捨ては文字数で数える");
}

/// start_line がファイル末尾を越えたら行は無い（空ページ）。
#[test]
fn line_reader_start_past_end_is_empty() {
    let (_dir, ws) = temp_workspace();
    ws.write_file("f.txt", "a\nb\nc").unwrap();
    let (mut reader, _total) = ws.line_reader("f.txt", 100, 512).unwrap();
    assert!(reader.next_line().unwrap().is_none());
}

/// 32KiB 窓（[`RANGE_SCAN_BYTE_CAP`]）の境界に 4 バイト文字を跨がせ、割れたバイトが carry で
/// 次窓と正しく結合されることを**相 A（デコード経路）**で確かめる。max_chars を大きく取り、
/// 境界越えを `text` に含める。
#[test]
fn line_reader_multibyte_split_across_window_phase_a() {
    let (_dir, ws) = temp_workspace();
    // "a"×32766 + "𠀀"(4B: 先頭 2 バイトが窓1・残り 2 バイトが窓2) + "bc"。改行なしの 1 行。
    let mut s = "a".repeat(RANGE_SCAN_BYTE_CAP - 2);
    s.push('𠀀');
    s.push_str("bc");
    ws.write_file("split.txt", &s).unwrap();

    let (mut reader, _t) = ws.line_reader("split.txt", 1, 100_000).unwrap();
    let l = reader.next_line().unwrap().unwrap();
    assert_eq!(l.overflow_chars, 0);
    assert_eq!(l.text.chars().count(), (RANGE_SCAN_BYTE_CAP - 2) + 1 + 2);
    assert!(
        l.text.ends_with("𠀀bc"),
        "窓境界で割れた 4 バイト文字が正しく復元される"
    );
}

/// 同じ窓境界の割れ文字を、**相 B（高速カウント経路）**でも 1 度だけ数える（carry の持ち越し
/// +1 と、次窓の継続バイトを数えないことで二重計上も欠落もしない）。max_chars を小さく取り、
/// 割れ文字を超過ぶん（overflow）のカウント対象にする。
#[test]
fn line_reader_multibyte_split_across_window_fast_path() {
    let (_dir, ws) = temp_workspace();
    // "a"×32766 + "𠀀" + "a"×10。改行なしの 1 行。max_chars=512 なので 𠀀 は相 B で数える。
    let mut s = "a".repeat(RANGE_SCAN_BYTE_CAP - 2);
    s.push('𠀀');
    s.push_str(&"a".repeat(10));
    ws.write_file("split2.txt", &s).unwrap();

    let (mut reader, _t) = ws.line_reader("split2.txt", 1, 512).unwrap();
    let l = reader.next_line().unwrap().unwrap();
    let total = (RANGE_SCAN_BYTE_CAP - 2) + 1 + 10;
    assert_eq!(l.text.chars().count(), 512);
    assert_eq!(
        l.overflow_chars,
        total - 512,
        "割れ文字が二重計上/欠落しない"
    );
}

/// #617（2 巡目）: 相 A が不正 UTF-8 の先頭バイトで詰まらない。`0xFF` 始まりで改行の無い数 MB の
/// 行を通しても `carry` が非有界に伸びず（＝相 B へ移行してメモリが窓 + max_chars で頭打ち）、
/// 各不正バイトは 1 文字（U+FFFD 相当）として数えられて返る。修正前はこの行全体が `carry` に
/// 溜まり、`text` 空・`overflow_chars` 0 で返っていた（＝この test は修正前なら落ちる）。
#[test]
fn line_reader_invalid_utf8_does_not_accumulate_carry() {
    let (_dir, ws) = temp_workspace();
    // 3 MiB の 0xFF（不正 UTF-8）を 1 行（改行なし）。生バイトなので std::fs で直接書く。
    let n = 3 * 1024 * 1024;
    std::fs::write(ws.root().join("bin.dat"), vec![0xFFu8; n]).unwrap();

    let (mut reader, _t) = ws.line_reader("bin.dat", 1, 512).unwrap();
    let l = reader.next_line().unwrap().unwrap();
    assert_eq!(
        l.text.chars().count(),
        512,
        "先頭 512 文字ぶんだけ text に載る"
    );
    assert!(
        l.text.chars().all(|c| c == '\u{FFFD}'),
        "不正バイトは置換文字として積まれる"
    );
    assert_eq!(
        l.overflow_chars,
        n - 512,
        "各不正バイトが 1 文字として数えられる（carry に溜め込まない証拠）"
    );
    assert!(reader.next_line().unwrap().is_none());
}
