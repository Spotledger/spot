//! Property tests: arbitrary text survives the wire shapes untouched.

use proptest::prelude::*;
use spot_llm::{Message, Part, Request, ToolSpec};

proptest! {
    #[test]
    fn messages_round_trip(text in ".*", tool_use_id in ".*", content in ".*", is_error in any::<bool>()) {
        let mut message = Message::user(text.clone());
        message.push_text(text.clone());
        message.push(Part::ToolResult { tool_use_id: tool_use_id.clone(), content: content.clone(), is_error });
        prop_assert_eq!(message.text_content(), format!("{text}\n{text}"));
        let wire = serde_json::to_string(&message).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let back: Message = serde_json::from_str(&wire).map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(back, message);
    }

    #[test]
    fn requests_round_trip(
        model in "[a-z/\\-]{1,32}",
        request_id in ".*",
        system in proptest::option::of(".*"),
        temperature in proptest::option::of(any::<u16>()),
        max_output in proptest::option::of(any::<u32>()),
    ) {
        let request = Request::new(model.clone(), request_id)
            .with_tool(ToolSpec {
                name: "probe".to_owned(),
                description: "A probe tool.".to_owned(),
                input_schema: serde_json::json!({ "type": "object" }),
            });
        let request = match system {
            Some(system) => request.with_system(system),
            None => request,
        };
        let request = match temperature {
            Some(permille) => request.with_temperature_permille(permille),
            None => request,
        };
        let request = match max_output {
            Some(max) => request.with_max_output_tokens(max),
            None => request,
        };
        let wire = serde_json::to_string(&request).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let back: Request = serde_json::from_str(&wire).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let temp = back.temperature;
        let wire_value =
            serde_json::to_value(&request).map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(back, request);
        // The contract: temperature is an integer permille — it serialises, and no float
        // ever crosses the wire.
        match temp {
            Some(permille) => prop_assert_eq!(
                wire_value["temperature"].clone(),
                serde_json::Value::Number(permille.into()),
                "temperature stays an integer permille"
            ),
            None => prop_assert!(wire_value.get("temperature").is_none()),
        }
    }
}
