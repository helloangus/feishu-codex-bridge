//! Feishu Card JSON 2.0 renderer. Layout metadata never reaches button schema.
use bridge_core::{
    command::Command,
    view::{ButtonAction, ButtonStyle, Panel, Tone},
};
use serde_json::{Value, json};

pub fn action_value(action: &ButtonAction) -> Value {
    match action {
        ButtonAction::Interaction { token, choice } => {
            json!({"command":"/interaction","token":token,"choice":choice})
        }
        ButtonAction::Command(command) => match command {
            Command::Help => json!({"command":"/help"}),
            Command::Status => json!({"command":"/status"}),
            Command::New => json!({"command":"/new"}),
            Command::Archived => json!({"command":"/archived"}),
            Command::Models => json!({"command":"/models"}),
            Command::Compact => json!({"command":"/compact"}),
            Command::ChangeDirectory(path) => json!({"command":"/cd","path":path}),
            Command::ConfirmDirectory(token) => json!({"command":"/cd-confirm","token":token}),
            Command::Resume(id) => json!({"command":"/resume","thread_id":id}),
            Command::Archive(id) => json!({"command":"/archive","thread_id":id}),
            Command::Unarchive(id) => json!({"command":"/unarchive","thread_id":id}),
            Command::Model(model) => json!({"command":"/model","model":model}),
            Command::Plan(Some(enabled)) => json!({"command":"/plan-toggle","enabled":enabled}),
            Command::Plan(None) => json!({"command":"/plan"}),
            Command::Stop(task) => json!({"command":"/stop","task_id":task}),
            Command::Approve { token, allow } => {
                json!({"command":"/interaction","token":token,"choice":if *allow { "allow" } else { "deny" }})
            }
            // Pairing secrets are never embedded in a card callback.
            Command::Pair(_) => json!({"command":"/help"}),
        },
    }
}

pub fn render(panel: &Panel) -> Value {
    let mut elements = vec![
        json!({"tag":"markdown","content":if panel.body.is_empty() { "（无内容）" } else { &panel.body }}),
    ];
    let mut last_group: Option<&str> = None;
    for item in &panel.buttons {
        let group = item.group.as_deref().filter(|group| !group.is_empty());
        let new_group = group.is_some() && group != last_group;
        if elements.len() == 1
            || item.section.is_some()
            || item.description.is_some()
            || item.separate
            || new_group
        {
            elements.push(json!({"tag":"hr"}));
            last_group = None;
        }
        if let Some(section) = &item.section {
            elements.push(json!({"tag":"markdown","content":format!("**{section}**")}));
        }
        let button = json!({"tag":"button","width":"fill","text":{"tag":"plain_text","content":item.label},
            "type":match item.style { ButtonStyle::Default=>"default",ButtonStyle::Primary=>"primary",ButtonStyle::Destructive=>"danger" },
            "behaviors":[{"type":"callback","value":action_value(&item.action)}]});
        if let Some(description) = &item.description {
            elements.push(json!({"tag":"markdown","content":description}));
            elements.push(button);
            last_group = None;
        } else if let Some(group) = group {
            if last_group != Some(group) {
                elements.push(json!({"tag":"markdown","content":group}));
                elements.push(json!({"tag":"column_set","horizontal_spacing":"8px","columns":[]}));
            }
            if let Some(columns) = elements
                .last_mut()
                .and_then(|value| value.get_mut("columns"))
                .and_then(Value::as_array_mut)
            {
                columns.push(
                    json!({"tag":"column","width":"weighted","weight":1,"elements":[button]}),
                );
            }
            last_group = Some(group);
        } else {
            elements.push(button);
            last_group = None;
        }
    }
    json!({"schema":"2.0","header":{"template":match panel.tone { Tone::Info=>"blue",Tone::Success=>"green",Tone::Warning=>"yellow",Tone::Error=>"red",Tone::Muted=>"grey" },"title":{"tag":"plain_text","content":panel.title}},"body":{"elements":elements}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_core::view::Button;
    #[test]
    fn groups_long_descriptions_and_stop_keep_mobile_layout() {
        let make = |label: &str, group: Option<&str>| Button {
            label: label.into(),
            description: None,
            section: None,
            group: group.map(str::to_owned),
            separate: false,
            style: ButtonStyle::Default,
            action: ButtonAction::Command(Command::Status),
        };
        let mut panel = Panel::text("控制面板", "说明", Tone::Info);
        panel.buttons = vec![make("状态", Some("任务")), make("模型", Some("任务"))];
        let mut long = make("恢复", None);
        long.description = Some("long thread name".into());
        panel.buttons.push(long);
        let mut stop = make("停止", None);
        stop.section = Some("停止任务".into());
        panel.buttons.push(stop);
        let card = render(&panel);
        let elements = card["body"]["elements"].as_array();
        assert!(elements.is_some());
        let serialized = card.to_string();
        assert!(!serialized.contains("_group"));
        assert!(!serialized.contains("\"section\""));
        assert!(serialized.contains("column_set"));
        assert!(serialized.contains("\"schema\":\"2.0\""));
        assert_eq!(
            card["body"]["elements"][3]["columns"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }
    #[test]
    fn pairing_secret_never_enters_card_payload() {
        assert_eq!(
            action_value(&ButtonAction::Command(Command::Pair("secret".into()))),
            json!({"command":"/help"})
        );
    }
}
