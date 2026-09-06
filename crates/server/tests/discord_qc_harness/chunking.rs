use super::support::*;

// ==================== (f) 2000 字超 say は複数チャンクで逐次配送される ====================

#[tokio::test]
async fn scenario_f_long_say_is_split_into_multiple_chunks() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    let body = long_say_body();
    assert!(
        body.chars().count() > 2000,
        "テスト前提: 本文は 2000 字超（{} 字）",
        body.chars().count()
    );
    let last_token = format!("LONGSAYLINE{:03}", LONGSAY_LINES - 1);

    fixture.append_message("704", &format!("{M_LONGSAY} 長文で返事して"));

    // 逐次送信の完了＝最後の行トークンを含む say チャンクが dry-run に現れる。
    let ok = {
        let buf = buf.clone();
        let last_token = last_token.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.body.contains(&last_token))
        })
        .await
    };
    assert!(ok, "長文 say の最終チャンクが出ない: {:?}", captured(&buf));

    // このシナリオの say チャンク（LONGSAYLINE を含む）を送信順に収集。
    let chunks: Vec<String> = captured(&buf)
        .into_iter()
        .filter(|c| c.kind == "say" && c.body.contains("LONGSAYLINE"))
        .map(|c| c.body)
        .collect();

    assert!(
        chunks.len() >= 2,
        "2000 字超 say が複数チャンクに分割されていない: {} チャンク",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            c.chars().count() <= 2000,
            "チャンクが Discord 上限 2000 字を超過: {} 字",
            c.chars().count()
        );
    }
    // 順序保証・欠落なし: チャンクを改行連結すると原文へ戻る（行優先分割）。
    assert_eq!(
        chunks.join("\n"),
        body,
        "分割チャンクの連結が原文と不一致（順序 or 欠落）"
    );
}
