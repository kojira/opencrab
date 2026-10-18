// Conformance harness waits (split out of support.rs for the file-size limit).

async fn wait_client_bound(client: &InstanceClient, address: &str, binding_id: &str) {
    for _ in 0..80 {
        if client.binding_for_address(address).await.as_deref() == Some(binding_id) {
            return;
        }
        tokio::time::advance(Duration::from_millis(5)).await;
    }
    panic!("bind ack");
}
/// live entry が消える（切断が反映される）まで待つ。
async fn wait_not_live(h: &Harness, instance_id: &str) {
    for _ in 0..200 {
        if h.state.lock_registry().unwrap().get(instance_id).is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("live entry が消えない: {instance_id}");
}
