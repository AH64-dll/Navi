//! Tool registration, schema validation, and call dispatch.
//!
//! A [`ToolRouter`] owns a [`ToolRegistry`] of [`ToolHandler`] implementations.
//! Each handler advertises a [`ToolDefinition`] whose `parameters` value is a
//! JSON schema. Before a call reaches its handler, the router checks that every
//! field listed in the schema's `required` array is present in the call's
//! arguments. Unknown tools, invalid arguments, and handler failures are all
//! reported as a structured [`ToolResult`] with `is_error` set, never as a
//! panic.

use std::collections::BTreeMap;

use navi_types::{ContentBlock, NaviResult, ToolCall, ToolDefinition, ToolResult};
use serde_json::Value;

use crate::agent_loop::ToolExecutor;

/// A callable tool that advertises a JSON-schema definition.
///
/// Implementations only need to describe themselves and execute already
/// validated arguments; the router supplies call identity and error framing.
pub trait ToolHandler {
    /// Returns the definition advertised to the model, including its schema.
    fn definition(&self) -> ToolDefinition;

    /// Executes the tool with arguments that passed required-field validation.
    ///
    /// A failure is returned as a [`NaviError`] so the router can convert it
    /// into an error [`ToolResult`] instead of unwinding.
    fn execute(&self, args: &Value) -> NaviResult<String>;

    /// Returns the tool name from the advertised definition.
    fn name(&self) -> String {
        self.definition().name
    }
}

/// A handler paired with the definition captured when it was registered.
struct RegisteredTool {
    definition: ToolDefinition,
    handler: Box<dyn ToolHandler>,
}

/// An ordered collection of tool handlers keyed by tool name.
///
/// Names come from [`ToolHandler::definition`]. Registering an existing name
/// replaces the previous handler, so the last registration wins. Iteration and
/// definition listing follow name order for deterministic output.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `handler` under the name in its definition.
    ///
    /// Re-registering a name replaces the earlier handler.
    pub fn register<H>(&mut self, handler: H)
    where
        H: ToolHandler + 'static,
    {
        let definition = handler.definition();
        self.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                handler: Box::new(handler),
            },
        );
    }

    /// Returns the definitions of all registered tools in name order.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    /// Returns the handler registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&dyn ToolHandler> {
        self.tools.get(name).map(|tool| tool.handler.as_ref())
    }

    /// Returns the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Returns `true` when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Routes model-issued [`ToolCall`]s to registered handlers.
#[derive(Default)]
pub struct ToolRouter {
    registry: ToolRegistry,
}

impl ToolRouter {
    /// Creates an empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler, replacing any earlier handler with the same name.
    pub fn register<H>(&mut self, handler: H)
    where
        H: ToolHandler + 'static,
    {
        self.registry.register(handler);
    }

    /// Returns the definitions to advertise to the model.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.registry.definitions()
    }

    /// Returns the underlying registry.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Executes `call` and returns a structured result.
    ///
    /// Validation runs before the handler is invoked. Any unknown tool, schema
    /// violation, or handler failure becomes a `ToolResult` with `is_error`
    /// set; the call id and name are preserved so the model can correlate the
    /// result with its request.
    pub fn dispatch(&self, call: &ToolCall) -> ToolResult {
        let Some(tool) = self.registry.tools.get(&call.name) else {
            return error_result(call, format!("unknown tool '{}'", call.name));
        };

        if let Err(message) = validate_required_fields(&tool.definition.parameters, &call.arguments)
        {
            return error_result(call, message);
        }

        match tool.handler.execute(&call.arguments) {
            Ok(content) => ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: vec![ContentBlock::Text { text: content }],
                is_error: false,
                details: None,
            },
            Err(err) => error_result(call, err.to_string()),
        }
    }
}

impl ToolExecutor for ToolRouter {
    fn execute(&mut self, call: &ToolCall) -> ToolResult {
        self.dispatch(call)
    }
}

/// Builds the error result used for every pre-handler failure.
fn error_result(call: &ToolCall, message: String) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![ContentBlock::Text { text: message }],
        is_error: true,
        details: None,
    }
}

/// Checks that every field named in a schema's `required` array is present in
/// `args`.
///
/// Only presence is checked, matching JSON Schema semantics: a required field
/// with a `null` value is present. Extra fields are allowed. A schema without a
/// `required` key requires nothing. A malformed `required` value is reported as
/// an error so a bad tool definition cannot silently skip validation.
pub fn validate_required_fields(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(required) = schema.get("required") else {
        return Ok(());
    };
    let Some(entries) = required.as_array() else {
        return Err("invalid tool schema: 'required' must be an array".to_owned());
    };

    let mut missing: Vec<&str> = Vec::new();
    for entry in entries {
        let Some(field) = entry.as_str() else {
            return Err("invalid tool schema: 'required' entries must be strings".to_owned());
        };
        if args.get(field).is_none() && !missing.contains(&field) {
            missing.push(field);
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    let fields = missing
        .iter()
        .map(|field| format!("'{field}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!("missing required field(s): {fields}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_types::NaviError;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestHandler {
        definition: ToolDefinition,
        calls: Arc<AtomicUsize>,
        response: NaviResult<String>,
    }

    impl TestHandler {
        fn new(name: &str, required: &[&str], response: NaviResult<String>) -> Self {
            Self::with_counter(name, required, response, Arc::new(AtomicUsize::new(0)))
        }

        fn with_counter(
            name: &str,
            required: &[&str],
            response: NaviResult<String>,
            calls: Arc<AtomicUsize>,
        ) -> Self {
            let required_values = required
                .iter()
                .map(|field| Value::String((*field).to_owned()))
                .collect::<Vec<_>>();
            Self {
                definition: ToolDefinition {
                    name: name.to_owned(),
                    description: format!("{name} test tool"),
                    parameters: json!({
                        "type": "object",
                        "properties": {},
                        "required": required_values,
                    }),
                },
                calls,
                response,
            }
        }
    }

    impl ToolHandler for TestHandler {
        fn definition(&self) -> ToolDefinition {
            self.definition.clone()
        }

        fn execute(&self, _args: &Value) -> NaviResult<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.response.clone()
        }
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
        }
    }

    fn text(result: &ToolResult) -> &str {
        match result.content.first() {
            Some(ContentBlock::Text { text }) => text,
            _ => "",
        }
    }

    #[test]
    fn definitions_are_listed_in_name_order() {
        let mut router = ToolRouter::new();
        router.register(TestHandler::new("zeta", &[], Ok("z".to_owned())));
        router.register(TestHandler::new("alpha", &[], Ok("a".to_owned())));

        let definitions = router.definitions();
        let names = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(definitions[0].description, "alpha test tool");
        assert_eq!(definitions[0].parameters["required"], json!([]),);
        assert!(router.registry().get("zeta").is_some());
        assert!(router.registry().get("missing").is_none());
    }

    #[test]
    fn dispatch_success_returns_text_with_call_identity() {
        let mut router = ToolRouter::new();
        router.register(TestHandler::new(
            "echo",
            &["path"],
            Ok("file contents".to_owned()),
        ));

        let result = router.dispatch(&call("call-7", "echo", json!({"path": "a.txt"})));

        assert!(!result.is_error);
        assert_eq!(result.tool_call_id, "call-7");
        assert_eq!(result.tool_name, "echo");
        assert_eq!(text(&result), "file contents");
        assert!(result.details.is_none());
    }

    #[test]
    fn dispatch_unknown_tool_returns_error_result() {
        let router = ToolRouter::new();

        let result = router.dispatch(&call("c1", "nope", json!({})));

        assert!(result.is_error);
        assert_eq!(result.tool_call_id, "c1");
        assert_eq!(result.tool_name, "nope");
        assert!(text(&result).contains("nope"));
    }

    #[test]
    fn dispatch_missing_required_field_skips_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut router = ToolRouter::new();
        router.register(TestHandler::with_counter(
            "write",
            &["path", "content"],
            Ok("written".to_owned()),
            Arc::clone(&calls),
        ));

        let result = router.dispatch(&call("c2", "write", json!({"path": "a.txt"})));

        assert!(result.is_error);
        assert!(text(&result).contains("content"));
        assert!(!text(&result).contains("'path'"));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dispatch_non_object_arguments_are_rejected_before_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut router = ToolRouter::new();
        router.register(TestHandler::with_counter(
            "read",
            &["path"],
            Ok("contents".to_owned()),
            Arc::clone(&calls),
        ));

        let result = router.dispatch(&call("c6", "read", json!(null)));

        assert!(result.is_error);
        assert_eq!(result.tool_call_id, "c6");
        assert!(text(&result).contains("'path'"));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dispatch_propagates_handler_error() {
        let mut router = ToolRouter::new();
        router.register(TestHandler::new(
            "bash",
            &[],
            Err(NaviError::PermissionDenied("sandbox blocked".to_owned())),
        ));

        let result = router.dispatch(&call("c3", "bash", json!({})));

        assert!(result.is_error);
        assert_eq!(result.tool_call_id, "c3");
        assert!(text(&result).contains("sandbox blocked"));
    }

    #[test]
    fn validate_required_fields_accepts_present_and_extra_fields() {
        let schema = json!({"required": ["path", "limit"]});
        let args = json!({"path": "a.txt", "limit": null, "extra": true});

        assert_eq!(validate_required_fields(&schema, &args), Ok(()));
        assert_eq!(validate_required_fields(&json!({}), &json!({})), Ok(()));
        assert_eq!(validate_required_fields(&json!(null), &json!([])), Ok(()));
    }

    #[test]
    fn validate_required_fields_rejects_missing_fields_without_duplicates() {
        let schema = json!({"required": ["a", "b", "a"]});
        let args = json!({"a": 1});

        let error = validate_required_fields(&schema, &args).expect_err("b is missing");
        assert_eq!(error, "missing required field(s): 'b'");

        let error =
            validate_required_fields(&schema, &json!("not an object")).expect_err("no fields");
        assert!(error.contains("'a'"));
        assert!(error.contains("'b'"));
        assert_eq!(error.matches("'a'").count(), 1);
    }

    #[test]
    fn validate_required_fields_rejects_malformed_schema() {
        let error = validate_required_fields(&json!({"required": "path"}), &json!({}))
            .expect_err("required must be an array");
        assert!(error.contains("required"));

        let error = validate_required_fields(&json!({"required": [1]}), &json!({}))
            .expect_err("required entries must be strings");
        assert!(error.contains("strings"));
    }

    #[test]
    fn duplicate_registration_replaces_previous_handler() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let mut router = ToolRouter::new();
        router.register(TestHandler::with_counter(
            "read",
            &[],
            Ok("first".to_owned()),
            Arc::clone(&first_calls),
        ));
        router.register(TestHandler::new("read", &[], Ok("second".to_owned())));

        assert_eq!(router.definitions().len(), 1);
        assert!(!router.registry().is_empty());
        assert_eq!(router.registry().len(), 1);
        let result = router.dispatch(&call("c4", "read", json!({})));
        assert_eq!(text(&result), "second");
        assert_eq!(first_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn router_can_execute_as_agent_loop_tool_executor() {
        let mut router = ToolRouter::new();
        router.register(TestHandler::new("echo", &["value"], Ok("done".to_owned())));

        let result = ToolExecutor::execute(&mut router, &call("c5", "echo", json!({"value": "x"})));

        assert!(!result.is_error);
        assert_eq!(text(&result), "done");
    }
}
