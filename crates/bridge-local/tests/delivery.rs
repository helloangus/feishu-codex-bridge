#![allow(clippy::unwrap_used)] // Assertions and fake recording locks only.
use bridge_app::messaging::*;
use bridge_core::view::Panel;
use bridge_local::delivery::Delivery;
use std::{
    fs::{self, File},
    io::{Read, Write},
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Fake {
    records: Mutex<Vec<String>>,
    bytes: Vec<u8>,
    fail: bool,
    reported_size: Option<u64>,
}
impl Messenger for Fake {
    fn send_text(&self, _: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.records.lock().unwrap().push(text);
            Ok(())
        })
    }
    fn send_panel(&self, _: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async move {
            self.records
                .lock()
                .unwrap()
                .push(format!("{} {}", panel.title, panel.body));
            Ok(MessageId("card".into()))
        })
    }
    fn update_panel(&self, _: MessageId, _: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn upload(
        &self,
        _: String,
        name: String,
        mut file: File,
        _: ResourceKind,
    ) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|_| DeliveryError::LocalIo)?;
            self.records
                .lock()
                .unwrap()
                .push(format!("upload:{name}:{}", bytes.len()));
            if self.fail {
                Err(DeliveryError::Transport)
            } else {
                Ok(())
            }
        })
    }
}
impl ResourceFetcher for Fake {
    fn download(&self, _: ResourceRef, mut destination: File) -> DeliveryFuture<'_, u64> {
        Box::pin(async move {
            destination
                .write_all(&self.bytes)
                .map_err(|_| DeliveryError::LocalIo)?;
            if self.fail {
                Err(DeliveryError::Transport)
            } else {
                Ok(self.reported_size.unwrap_or(self.bytes.len() as u64))
            }
        })
    }
}
fn attachment(kind: ResourceKind) -> Attachment {
    Attachment {
        name: "../../测试.png".into(),
        resource: ResourceRef {
            message_id: "message".into(),
            key: "resource".into(),
            kind,
        },
    }
}
type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn stage_images_then_diff_and_upload_only_changed_artifacts() -> Result {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let fake = Arc::new(Fake {
        bytes: b"\x89PNG\r\n\x1a\nbody".to_vec(),
        ..Fake::default()
    });
    let delivery =
        Delivery::new(fake.clone(), fake.clone()).excluding(vec![root.join("custom-state")]);
    fs::write(root.join("main.rs"), "old\n")?;
    fs::write(root.join("same.pdf"), b"same")?;
    let prepared = delivery
        .prepare_files(
            "task".into(),
            root.into(),
            vec![attachment(ResourceKind::Image)],
        )
        .await?;
    assert_eq!(prepared.images.len(), 1);
    assert_eq!(
        prepared.images[0].parent(),
        Some(root.join("feishu-inbox").as_path())
    );
    assert_eq!(fs::read(&prepared.images[0])?, fake.bytes);
    assert!(
        prepared
            .prompt
            .contains(prepared.images[0].to_str().unwrap())
    );
    fs::write(root.join("main.rs"), "new\n```\n")?;
    fs::write(root.join("report.pdf"), b"pdf")?;
    for dir in ["custom-state", "target"] {
        fs::create_dir(root.join(dir))?;
        fs::write(root.join(dir).join("private.pdf"), b"secret")?;
    }
    fs::write(root.join(".env"), b"secret")?;
    delivery
        .finish_files("task".into(), "chat".into(), root.into())
        .await?;
    let records = fake.records.lock().unwrap().clone();
    assert!(
        records
            .iter()
            .any(|s| s.contains("-old") && s.contains("+new") && s.contains("````diff"))
    );
    assert_eq!(
        records.iter().filter(|s| s.starts_with("upload:")).count(),
        1
    );
    assert!(records.iter().any(|s| s == "upload:report.pdf:3"));
    assert!(
        !records
            .iter()
            .any(|s| s.contains("secret") || s.contains("private.pdf") || s.contains("same.pdf"))
    );
    drop(records);
    delivery
        .finish_files("task".into(), "chat".into(), root.into())
        .await?;
    assert_eq!(
        fake.records
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("upload:"))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn failed_or_invalid_download_never_publishes_partial_files() -> Result {
    for fail in [false, true] {
        let temp = tempfile::tempdir()?;
        let fake = Arc::new(Fake {
            bytes: b"not an image".to_vec(),
            fail,
            ..Fake::default()
        });
        let delivery = Delivery::new(fake.clone(), fake);
        assert!(
            delivery
                .prepare_files(
                    "task".into(),
                    temp.path().into(),
                    vec![attachment(ResourceKind::Image)]
                )
                .await
                .is_err()
        );
        assert_eq!(fs::read_dir(temp.path().join("feishu-inbox"))?.count(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_files_use_prompt_paths_and_oversized_downloads_are_discarded() -> Result {
    let temp = tempfile::tempdir()?;
    let fake = Arc::new(Fake {
        bytes: b"plain data".to_vec(),
        ..Fake::default()
    });
    let delivery = Delivery::new(fake.clone(), fake);
    let result = delivery
        .prepare_files(
            "file".into(),
            temp.path().into(),
            vec![attachment(ResourceKind::File)],
        )
        .await?;
    assert!(result.images.is_empty());
    assert!(result.prompt.contains("feishu-inbox"));
    assert_eq!(fs::read_dir(temp.path().join("feishu-inbox"))?.count(), 1);
    let fake = Arc::new(Fake {
        bytes: b"partial".to_vec(),
        reported_size: Some(21 * 1024 * 1024),
        ..Fake::default()
    });
    let delivery = Delivery::new(fake.clone(), fake);
    assert!(matches!(
        delivery
            .prepare_files(
                "oversize".into(),
                temp.path().into(),
                vec![attachment(ResourceKind::File)]
            )
            .await,
        Err(DeliveryError::TooLarge)
    ));
    assert_eq!(fs::read_dir(temp.path().join("feishu-inbox"))?.count(), 1);
    Ok(())
}

#[tokio::test]
async fn inbox_symlink_rejected_and_upload_failure_reported_once() -> Result {
    let temp = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::os::unix::fs::symlink(outside.path(), temp.path().join("feishu-inbox"))?;
    let fake = Arc::new(Fake {
        fail: true,
        ..Fake::default()
    });
    let delivery = Delivery::new(fake.clone(), fake.clone());
    assert!(
        delivery
            .prepare_files(
                "bad".into(),
                temp.path().into(),
                vec![attachment(ResourceKind::File)]
            )
            .await
            .is_err()
    );
    assert_eq!(fs::read_dir(outside.path())?.count(), 0);
    delivery
        .prepare_files("task".into(), temp.path().into(), vec![])
        .await?;
    fs::write(temp.path().join("report.pdf"), b"pdf")?;
    delivery
        .finish_files("task".into(), "chat".into(), temp.path().into())
        .await?;
    let records = fake.records.lock().unwrap();
    assert_eq!(
        records.iter().filter(|s| s.starts_with("upload:")).count(),
        1
    );
    assert!(records.iter().any(|s| s.contains("发送失败")));
    Ok(())
}

#[tokio::test]
async fn oversized_artifacts_do_not_consume_the_ten_delivery_slots() -> Result {
    let temp = tempfile::tempdir()?;
    let fake = Arc::new(Fake::default());
    let delivery = Delivery::new(fake.clone(), fake.clone());
    delivery
        .prepare_files("task".into(), temp.path().into(), vec![])
        .await?;
    File::create(temp.path().join("a.pdf"))?.set_len(21 * 1024 * 1024)?;
    for i in 0..11 {
        fs::write(temp.path().join(format!("b{i:02}.pdf")), format!("pdf{i}"))?;
    }
    delivery
        .finish_files("task".into(), "chat".into(), temp.path().into())
        .await?;
    let records = fake.records.lock().unwrap();
    assert_eq!(
        records.iter().filter(|s| s.starts_with("upload:")).count(),
        10
    );
    assert!(records.iter().any(|s| s.contains("另有 2 项")));
    Ok(())
}

#[tokio::test]
async fn generated_images_are_thread_scoped_and_deduplicated_with_workspace() -> Result {
    let temp = tempfile::tempdir()?;
    let generated = tempfile::tempdir()?;
    fs::create_dir(generated.path().join("thread"))?;
    fs::write(generated.path().join("thread/old.png"), b"old")?;
    let fake = Arc::new(Fake::default());
    let delivery =
        Delivery::new(fake.clone(), fake.clone()).generated_images(Some(generated.path().into()));
    delivery
        .prepare_files("task".into(), temp.path().into(), vec![])
        .await?;
    delivery.bind_files("task".into(), "thread".into()).await?;
    fs::write(generated.path().join("thread/new.png"), b"new")?;
    fs::write(temp.path().join("copy.png"), b"new")?;
    fs::create_dir(generated.path().join("foreign"))?;
    fs::write(generated.path().join("foreign/private.png"), b"private")?;
    delivery
        .finish_files("task".into(), "chat".into(), temp.path().into())
        .await?;
    let records = fake.records.lock().unwrap();
    assert_eq!(
        records.iter().filter(|s| s.starts_with("upload:")).count(),
        1
    );
    assert!(
        !records
            .iter()
            .any(|s| s.contains("old.png") || s.contains("private"))
    );
    Ok(())
}
