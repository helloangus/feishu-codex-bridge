use bridge_app::messaging::ResourceKind;
use bridge_feishu::{attachments, message_text};
use serde_json::json;

#[test]
fn post_extracts_caption_and_deduplicates_only_selected_locale() {
    let content = json!({"zh_cn":{"title":"检查附件","content":[[
        {"tag":"text","text":"说明"},{"tag":"img","image_key":"one"},
        {"tag":"img","image_key":"one"},{"tag":"file","file_key":"two","file_name":"../data.csv"}
    ]]},"en_us":{"content":[[{"tag":"img","image_key":"foreign"}]]}});
    let files = attachments("message", "post", &content);
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].resource.message_id, "message");
    assert_eq!(files[0].resource.kind, ResourceKind::Image);
    assert_eq!(files[1].resource.key, "two");
    assert!(
        message_text("post", &content)
            .is_some_and(|s| s.contains("检查附件") && s.contains("说明"))
    );
}

#[test]
fn overflow_is_visible_to_runtime_and_empty_keys_are_ignored() {
    let nodes: Vec<_> = (0..12)
        .map(|i| json!({"tag":"img","image_key":format!("image{i}")}))
        .collect();
    assert_eq!(
        attachments("m", "post", &json!({"content":[nodes]})).len(),
        11
    );
    assert!(attachments("m", "image", &json!({"image_key":""})).is_empty());
    assert_eq!(
        attachments("m", "video", &json!({"media_key":"v"})).len(),
        1
    );
}
