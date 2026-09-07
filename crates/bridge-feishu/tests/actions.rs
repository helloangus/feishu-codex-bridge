use bridge_core::{command::Command, view::ButtonAction};
use bridge_feishu::{cards::action_value, decode_action};

#[test]
fn rendered_controls_round_trip_without_losing_arguments() -> Result<(), Box<dyn std::error::Error>>
{
    for command in [
        Command::Help,
        Command::Status,
        Command::New,
        Command::Archived,
        Command::Models,
        Command::Compact,
        Command::ChangeDirectory(Some("dir /stop".into())),
        Command::Resume(Some("thread".into())),
        Command::Resume(None),
        Command::Archive("thread".into()),
        Command::Unarchive("thread".into()),
        Command::Model(Some("model".into())),
        Command::Model(None),
        Command::Plan(None),
        Command::Plan(Some(false)),
        Command::Stop(Some("task".into())),
    ] {
        let action = ButtonAction::Command(command);
        assert_eq!(decode_action(&action_value(&action).to_string())?, action);
    }
    let interaction = ButtonAction::Interaction {
        token: "opaque".into(),
        choice: "deny".into(),
    };
    assert_eq!(
        decode_action(&action_value(&interaction).to_string())?,
        interaction
    );
    Ok(())
}

#[test]
fn missing_interaction_and_archive_arguments_are_rejected() {
    for value in [
        r#"{"command":"/interaction","token":"","choice":"allow"}"#,
        r#"{"command":"/archive"}"#,
        r#"{"command":"/unarchive","thread_id":" "}"#,
    ] {
        assert!(decode_action(value).is_err());
    }
}
