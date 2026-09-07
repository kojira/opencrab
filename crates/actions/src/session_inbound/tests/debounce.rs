#[test]
fn privilege_debounce_flushes_each_interval() {
    let mut hold = debounce::PrivilegeDebounce::default();
    let now = tokio::time::Instant::now();
    hold.push("fast", 30, now);
    hold.push("slow", 300, now);
    assert_eq!(hold.intervals(), vec![30, 300]);
    let at_watch = hold.take_ready(now + Duration::from_secs(60));
    assert_eq!(at_watch.len(), 1);
    assert_eq!(at_watch[0].0, 30);
    assert_eq!(at_watch[0].1, vec!["fast"]);
    assert_eq!(hold.intervals(), vec![300]);
    let later = hold.take_ready(now + Duration::from_secs(300));
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].0, 300);
    assert_eq!(later[0].1, vec!["slow"]);
    assert!(hold.is_empty());
}

#[tokio::test(start_paused = true)]
async fn privilege_fire_emits_each_interval() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fire = PrivilegeFire::new(move |items: Vec<(String, CallerIdentity)>| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(items);
        }
    });
    fire.hold("fast".into(), CallerIdentity::Owner, 30);
    fire.hold("slow".into(), CallerIdentity::Agent, 300);
    tokio::time::advance(Duration::from_secs(30)).await;
    let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("30s で fast が発火する")
        .expect("channel open");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, "fast");
    assert_eq!(fire.held_intervals(), vec![300]);
    tokio::time::advance(Duration::from_secs(270)).await;
    let later = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("300s で slow が発火する")
        .expect("channel open");
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].0, "slow");
    assert_eq!(fire.held_len(), 0);
}
