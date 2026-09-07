use bridge_app::requests::AgentReply;
use bridge_codex::{RpcId, requests::prepare, transport::Connection};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn string_id_is_preserved_and_unknown_request_gets_error() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let mut connection = Connection::new(read, write, 7, 8192);
        let mut lines = BufReader::new(server).lines();
        let (_, handle) = prepare(
            connection.client.clone(),
            7,
            RpcId::String("server-q".into()),
            "item/fileChange/requestApproval",
            json!({"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1}),
        )
        .await?;
        handle.reply(AgentReply::Approve(false)).await?;
        let value: Value = serde_json::from_str(&lines.next_line().await?.ok_or("missing reply")?)?;
        assert_eq!(
            value,
            json!({"id":"server-q","result":{"decision":"decline"}})
        );
        assert!(
            prepare(
                connection.client.clone(),
                7,
                RpcId::Integer(4),
                "future/requestApproval",
                json!({})
            )
            .await
            .is_err()
        );
        let value: Value =
            serde_json::from_str(&lines.next_line().await?.ok_or("missing rejection")?)?;
        assert_eq!(value["id"], 4);
        assert_eq!(value["error"]["code"], -32602);
        assert!(value.get("result").is_none());
        assert!(
            prepare(
                connection.client.clone(),
                6,
                RpcId::Integer(5),
                "future/requestApproval",
                json!({})
            )
            .await
            .is_err()
        );
        connection.shutdown().await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}
