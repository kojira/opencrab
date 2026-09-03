/// 告知が名指しするツールと引数が、実在するツールの実在するパラメータと一致すること。
///
/// 本 PR で 2,400 件級の要約が文脈から消えるので、告知の 1 行が唯一の復旧導線になる。
/// **「失われていません、こう引けます」と書いて実際には引けない**のが最悪の壊れ方で、
/// 実際に一度そう書いていた（`retrieve_memory_nodes` に keyword / date range を渡せ、と
/// 書いたが、このツールは `node_ids` しか受け取らず、日付範囲を取る記憶検索ツールは
/// 存在しない）。文言とツール定義を突き合わせて固定する。
#[test]
fn omitted_notice_matches_the_real_tool_surface() {
    use opencrab_actions::memory_access::{RetrieveMemoryNodesAction, SearchMemoryIndexAction};
    use opencrab_actions::Action;

    let notice = super::past_summary_omitted_notice(42);
    let search = SearchMemoryIndexAction;
    let retrieve = RetrieveMemoryNodesAction;
    let props = |a: &dyn Action| -> Vec<String> {
        a.parameters()["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{} の parameters に properties が無い", a.name()))
            .keys()
            .cloned()
            .collect()
    };

    // 名指ししたツールは実在する（名前はツール定義から取る）。
    assert!(
        notice.contains(search.name()) && notice.contains(retrieve.name()),
        "告知が実在するツールを名指ししていない: {notice}"
    );
    // 渡せと書いた引数を、そのツールが実際に受け取る。
    assert!(
        notice.contains(&format!("{}(query)", search.name())),
        "検索の呼び方が書かれていない: {notice}"
    );
    assert!(
        props(&search).contains(&"query".to_string()),
        "{} は query を受け取らない: {:?}",
        search.name(),
        props(&search)
    );
    // retrieve_memory_nodes は node_ids しか受け取らない。告知はこのツールへ
    // キーワードや日付を渡すよう指示してはならない（＝名前より後ろに出さない）。
    assert_eq!(
        props(&retrieve),
        vec!["node_ids".to_string()],
        "retrieve_memory_nodes のパラメータが変わった。告知の文言を見直すこと"
    );
    let after = &notice[notice.find(retrieve.name()).unwrap()..];
    for forbidden in ["keyword", "date range", "date_range", "query"] {
        assert!(
            !after.contains(forbidden),
            "retrieve_memory_nodes に {forbidden} を渡すよう読める: {notice}"
        );
    }
}
