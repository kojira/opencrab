#[tokio::test]
async fn publishes_gpt_6_sol_and_luna_with_gpt_5_6_capabilities() {
    let models = ChatGptProvider::new().available_models().await.unwrap();

    for id in ["gpt-6-sol", "gpt-6-luna"] {
        let model = models.iter().find(|model| model.id == id).expect(id);
        assert_eq!(model.context_window, 400_000);
        assert!(model.supports_function_calling);
        assert!(model.supports_vision);
    }
}
