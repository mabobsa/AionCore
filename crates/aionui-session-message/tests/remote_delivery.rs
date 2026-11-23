//! Fork: session messages between machines (`service/remote.rs`).

mod common;

use std::sync::{Arc, Mutex};

use aionui_api_types::{
    SessionDeliveryStatus, SessionSendMessageRequest, SessionSendMessageResponse, SessionToolErrorCode,
};
use aionui_session_message::{
    RemoteSessionDelivery, RemoteSessionRelay, RemoteSessionRelayRequest, SessionMessageError,
};
use async_trait::async_trait;
use common::{USER, setup};

#[derive(Default)]
struct RecordingRelay {
    requests: Mutex<Vec<RemoteSessionRelayRequest>>,
}

#[async_trait]
impl RemoteSessionRelay for RecordingRelay {
    async fn relay(
        &self,
        _user_id: &str,
        request: RemoteSessionRelayRequest,
    ) -> Result<SessionSendMessageResponse, SessionMessageError> {
        let to = request.to.clone();
        self.requests.lock().unwrap().push(request);
        Ok(SessionSendMessageResponse {
            status: SessionDeliveryStatus::Queued,
            to,
        })
    }
}

fn request(to: &str, message: &str) -> SessionSendMessageRequest {
    SessionSendMessageRequest {
        to: to.to_owned(),
        message: message.to_owned(),
    }
}

fn delivery(from: &str, to: &str, message: &str) -> RemoteSessionDelivery {
    RemoteSessionDelivery {
        request: RemoteSessionRelayRequest {
            from_conversation_id: from.to_owned(),
            from_name: "Main conversation".to_owned(),
            from_workspace: Some("D:/work".to_owned()),
            to: to.to_owned(),
            message: message.to_owned(),
        },
        from_machine: "Main PC".to_owned(),
        from_machine_id: None,
        from_machine_role: None,
        from_conversation_url: None,
    }
}

#[tokio::test]
async fn a_target_missing_here_goes_to_the_relay_with_the_sender_details() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "Sender", "/w/a").await;
    let relay = Arc::new(RecordingRelay::default());
    ctx.service.set_remote_relay(relay.clone());

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_on_mac", "hello"))
        .await
        .expect("relayed");

    assert_eq!(response.status, SessionDeliveryStatus::Queued);
    assert_eq!(
        *relay.requests.lock().unwrap(),
        vec![RemoteSessionRelayRequest {
            from_conversation_id: from.id.clone(),
            from_name: "Sender".to_owned(),
            from_workspace: Some("/w/a".to_owned()),
            to: "conv_on_mac".to_owned(),
            message: "hello".to_owned(),
        }]
    );
}

#[tokio::test]
async fn without_a_relay_a_missing_target_is_still_not_found() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "Sender", "/w/a").await;

    let error = ctx
        .service
        .send(USER, &from.id, &request("conv_on_mac", "hello"))
        .await
        .unwrap_err();

    assert_eq!(error.code(), SessionToolErrorCode::TargetNotFound);
}

#[tokio::test]
async fn a_local_target_never_goes_to_the_relay() {
    let ctx = setup().await;
    let from = ctx.create_conversation("conv_from", "Sender", "/w/a").await;
    ctx.create_conversation("conv_target", "Target", "/w/a").await;
    let relay = Arc::new(RecordingRelay::default());
    ctx.service.set_remote_relay(relay.clone());

    let response = ctx
        .service
        .send(USER, &from.id, &request("conv_target", "local"))
        .await
        .expect("delivered");

    assert_eq!(response.status, SessionDeliveryStatus::Delivered);
    assert!(relay.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_relayed_message_is_delivered_with_the_sender_machine_in_the_block() {
    let ctx = setup().await;
    let target = ctx.create_conversation("conv_target", "Target", "/Users/me/work").await;

    let response = ctx
        .service
        .deliver_remote(USER, delivery("conv_main", &target.id, "hi from main"))
        .await
        .expect("delivered");

    assert_eq!(response.status, SessionDeliveryStatus::Delivered);
    let content = ctx.last_user_message_content(&target.id).await;
    assert!(content.starts_with("[[AION_SESSION_MESSAGE]]"), "{content}");
    assert!(content.contains("from: Main conversation\tconv_main"), "{content}");
    assert!(content.contains("reply_to: conv_main"), "{content}");
    assert!(
        content.contains("workspace: D:/work on another machine (Main PC); don't use relative paths"),
        "{content}"
    );
    assert!(content.trim_end().ends_with("hi from main"), "{content}");
    let source = source_metadata(&content);
    assert_eq!(source["machineName"], "Main PC");
    assert!(source["externalUrl"].is_null());
}

fn source_metadata(content: &str) -> serde_json::Value {
    let source = content
        .lines()
        .find_map(|line| line.strip_prefix("source: "))
        .expect("remote source metadata");
    serde_json::from_str(source).expect("valid source JSON")
}

#[tokio::test]
async fn relayed_messages_persist_distinct_machine_roles_and_web_destinations() {
    let ctx = setup().await;
    ctx.create_conversation("conv_target", "Target", "/work").await;
    for (id, role, name) in [
        ("main", "main", "메인 PC"),
        ("mac", "sub", "MacBook"),
        ("lab", "sub", "실험 PC"),
    ] {
        let mut message = delivery(&format!("conv_{id}"), "conv_target", "remote body");
        let url = format!("https://{id}.example/#/conversation/conv_{id}");
        message.from_machine = name.to_owned();
        message.from_machine_id = Some(id.to_owned());
        message.from_machine_role = Some(role.to_owned());
        message.from_conversation_url = Some(url.clone());
        ctx.service.deliver_remote(USER, message).await.expect("delivered");
        let source = source_metadata(&ctx.last_user_message_content("conv_target").await);
        assert_eq!(
            source,
            serde_json::json!({
                "machineId": id, "machineName": name, "machineRole": role, "externalUrl": url,
            })
        );
    }
}

#[test]
fn old_relays_remain_compatible_without_navigation_metadata() {
    let message: RemoteSessionDelivery = serde_json::from_value(serde_json::json!({
        "fromConversationId": "source", "fromName": "Sender", "to": "target", "message": "body", "fromMachine": "MacBook",
    })).unwrap();
    assert!(message.from_machine_id.is_none());
    assert!(message.from_machine_role.is_none());
    assert!(message.from_conversation_url.is_none());
}

#[tokio::test]
async fn a_relayed_message_never_takes_a_second_hop() {
    let ctx = setup().await;
    let relay = Arc::new(RecordingRelay::default());
    ctx.service.set_remote_relay(relay.clone());

    let error = ctx
        .service
        .deliver_remote(USER, delivery("conv_main", "conv_missing", "x"))
        .await
        .unwrap_err();

    assert_eq!(error.code(), SessionToolErrorCode::TargetNotFound);
    assert!(relay.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_relayed_message_keeps_the_team_toggle_and_schema_checks() {
    let ctx = setup().await;
    ctx.create_team_conversation("conv_team", "team-1").await;
    let team_error = ctx
        .service
        .deliver_remote(USER, delivery("conv_main", "conv_team", "x"))
        .await
        .unwrap_err();
    assert_eq!(team_error.code(), SessionToolErrorCode::TargetIsTeam);

    let empty_error = ctx
        .service
        .deliver_remote(USER, delivery("conv_main", "conv_team", "  "))
        .await
        .unwrap_err();
    assert_eq!(empty_error.code(), SessionToolErrorCode::SchemaValidationFailed);

    ctx.disable_feature(USER).await;
    let disabled_error = ctx
        .service
        .deliver_remote(USER, delivery("conv_main", "conv_team", "x"))
        .await
        .unwrap_err();
    assert_eq!(disabled_error.code(), SessionToolErrorCode::FeatureDisabled);
}
