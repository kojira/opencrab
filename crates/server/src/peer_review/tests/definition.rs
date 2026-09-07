    #[test]
    fn definition_is_stable() {
        let def = request_peer_review_definition();
        assert_eq!(def.name, "request_peer_review");
        assert!(def.description.starts_with("自分の成果物（diff・実行結果・トレース等）を、同じチャンネルにいる別のBot（別モデル）に"));
        assert_eq!(def.parameters["required"], json!(["content"]));
        let props = def.parameters["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = props.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["channel_id", "content", "instructions", "reviewer"]
        );
    }

    /// **#158 S2（#218）の成果を守る**: このツールは移設で全ターン（Nostr / web / REST /
    /// 定期実行）に露出するため、引数の説明文に transport 前提を書き戻してはならない。
    ///
    /// - `[Discord context]` を参照させると、その文脈が存在しないターンでモデルに宛先の
    ///   出所を教えることになり、幻覚した宛先への誤投稿を招く。
    /// - メンション記法（`<@`）を露出すると、名簿に無い識別子を組み立てて渡させる。
    ///   レビュアーは**表示名のみ**（記法の組み立ては transport の責務）。
    ///
    /// 説明文の先頭だけを見る [`definition_is_stable`] ではこの退行を検出できない
    /// （実際に rebase で 1 度巻き戻った）。ここは**全 description の本文**を見る。
    #[test]
    fn definition_text_stays_transport_neutral() {
        let def = request_peer_review_definition();
        let mut texts: Vec<String> = vec![def.description.clone()];
        for (name, prop) in def.parameters["properties"].as_object().unwrap() {
            let d = prop["description"].as_str().unwrap_or_default();
            assert!(!d.is_empty(), "{name} の説明文が空");
            texts.push(d.to_string());
        }
        for t in &texts {
            assert!(
                !t.contains("Discord context"),
                "存在しない文脈（[Discord context]）を参照させてはならない（#158 S2）: {t}"
            );
            assert!(
                !t.contains("<@"),
                "メンション記法は transport の責務。説明文に露出させない（#158 S2）: {t}"
            );
        }
        // 宛先は「省略が既定」であることを明示し続ける（#158 S1/S2）。
        let channel = def.parameters["properties"]["channel_id"]["description"]
            .as_str()
            .unwrap();
        assert!(channel.contains("通常は省略する"), "{channel}");
        assert!(
            channel.contains("推測した識別子を渡してはならない"),
            "{channel}"
        );
        // レビュアーは表示名のみ（transport のユーザー識別子を渡させない）。
        let reviewer = def.parameters["properties"]["reviewer"]["description"]
            .as_str()
            .unwrap();
        assert!(reviewer.contains("表示名を渡す"), "{reviewer}");
        assert!(!reviewer.contains("user id"), "{reviewer}");
    }

