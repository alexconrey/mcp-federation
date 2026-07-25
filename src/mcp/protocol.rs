use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema)]
#[schema(example = json!({
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/list",
    "params": {}
}))]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[schema(value_type = Option<serde_json::Value>)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub params: serde_json::Value,
}

#[derive(Serialize, ToSchema)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<serde_json::Value>)]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<serde_json::Value>)]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Serialize, ToSchema)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

pub fn success_response(request: &JsonRpcRequest, result: serde_json::Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: request.id.clone(),
        result: Some(result),
        error: None,
    }
}

pub fn error_response(request: &JsonRpcRequest, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: request.id.clone(),
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
        }),
    }
}

pub fn method_not_found(request: &JsonRpcRequest) -> JsonRpcResponse {
    error_response(
        request,
        -32601,
        &format!("Method not found: {}", request.method),
    )
}

pub fn notification_response() -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: None,
        result: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(method: &str, id: i32) -> JsonRpcRequest {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {}
        }))
        .unwrap()
    }

    #[test]
    fn deserialize_request_with_params() {
        let json = r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"list_pods","arguments":{}}}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "tools/call");
        assert_eq!(req.id, Some(serde_json::json!(42)));
        assert_eq!(req.params["name"], "list_pods");
    }

    #[test]
    fn deserialize_request_without_params() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "tools/list");
        assert!(req.params.is_null());
    }

    #[test]
    fn deserialize_notification_no_id() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "notifications/initialized");
        assert!(req.id.is_none());
    }

    #[test]
    fn success_response_preserves_id() {
        let req = make_request("tools/list", 99);
        let resp = success_response(&req, serde_json::json!({"tools": []}));

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 99);
        assert!(json["result"]["tools"].is_array());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn error_response_includes_code_and_message() {
        let req = make_request("tools/call", 7);
        let resp = error_response(&req, -32000, "something broke");

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], 7);
        assert_eq!(json["error"]["code"], -32000);
        assert_eq!(json["error"]["message"], "something broke");
        assert!(json.get("result").is_none());
    }

    #[test]
    fn method_not_found_uses_correct_code() {
        let req = make_request("bogus/method", 1);
        let resp = method_not_found(&req);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], -32601);
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bogus/method"));
    }

    #[test]
    fn notification_response_has_no_id() {
        let resp = notification_response();
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("id").is_none());
        assert!(json.get("result").is_none());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn success_response_serialization_roundtrip() {
        let req = make_request("initialize", 1);
        let resp = success_response(
            &req,
            serde_json::json!({
                "protocolVersion": "2025-11-25",
                "serverInfo": {"name": "test"}
            }),
        );

        let serialized = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["result"]["protocolVersion"], "2025-11-25");
    }
}
