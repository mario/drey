//! Helpers over raw JSON-RPC values.

use serde_json::{json, Value};

pub fn is_request(v: &Value) -> bool {
    v.get("id").is_some() && v.get("method").is_some()
}

pub fn is_response(v: &Value) -> bool {
    v.get("id").is_some() && v.get("method").is_none()
}

pub fn method(v: &Value) -> &str {
    v.get("method").and_then(Value::as_str).unwrap_or("")
}

/// Client request ids live in a per-client namespace. We rewrite them into one
/// server-facing namespace by prefixing the client id, then undo it on the way
/// back. The separator is a control character so it cannot collide with a
/// plausible string id.
const SEP: char = '\u{1}';

/// Prefix marking ids the proxy itself issued; responses to these are ours.
pub const INTERNAL: &str = "drey/";

pub fn encode_id(client: u64, original: &Value) -> Value {
    Value::String(format!("{client}{SEP}{original}"))
}

pub fn decode_id(encoded: &Value) -> Option<(u64, Value)> {
    let s = encoded.as_str()?;
    let (client, rest) = s.split_once(SEP)?;
    Some((client.parse().ok()?, serde_json::from_str(rest).ok()?))
}

pub fn is_internal_id(id: &Value) -> bool {
    id.as_str().is_some_and(|s| s.starts_with(INTERNAL))
}

pub fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub fn request(id: Value, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

pub fn encode(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).expect("serialising a Value cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trips_for_every_json_rpc_id_shape() {
        for original in [json!(1), json!(0), json!("abc"), json!("has spaces")] {
            let encoded = encode_id(42, &original);
            assert_eq!(decode_id(&encoded), Some((42, original)));
        }
    }

    #[test]
    fn decode_rejects_foreign_ids() {
        assert_eq!(decode_id(&json!(7)), None);
        assert_eq!(decode_id(&json!("drey/internal/3")), None);
    }

    #[test]
    fn internal_ids_are_recognised() {
        assert!(is_internal_id(&json!("drey/initialize")));
        assert!(!is_internal_id(&encode_id(1, &json!(2))));
    }

    #[test]
    fn message_shapes_are_distinguished() {
        let req = request(json!(1), "m", json!({}));
        let resp = response(json!(1), json!(null));
        let note = notification("m", json!({}));
        assert!(is_request(&req) && !is_response(&req));
        assert!(is_response(&resp) && !is_request(&resp));
        assert!(!is_request(&note) && !is_response(&note));
    }

    #[test]
    fn ids_from_different_clients_never_collide() {
        assert_ne!(encode_id(1, &json!(1)), encode_id(2, &json!(1)));
        // The separator is a control character, so a client id embedded in a
        // string id cannot forge another client's namespace.
        let sneaky = encode_id(1, &json!("2\u{1}3"));
        assert_eq!(decode_id(&sneaky), Some((1, json!("2\u{1}3"))));
    }

    #[test]
    fn decoding_recovers_the_id_type_not_just_its_text() {
        let numeric = decode_id(&encode_id(3, &json!(7))).unwrap().1;
        assert!(numeric.is_number());
        let textual = decode_id(&encode_id(3, &json!("7"))).unwrap().1;
        assert!(textual.is_string());
    }

    #[test]
    fn decode_rejects_a_non_numeric_client_prefix() {
        assert_eq!(decode_id(&json!("abc\u{1}1")), None);
        assert_eq!(decode_id(&json!("\u{1}1")), None);
        assert_eq!(decode_id(&json!("1\u{1}not json")), None);
        assert_eq!(decode_id(&json!(null)), None);
    }

    #[test]
    fn null_ids_round_trip_since_clients_may_send_them() {
        assert_eq!(
            decode_id(&encode_id(5, &json!(null))),
            Some((5, json!(null)))
        );
    }

    #[test]
    fn method_is_empty_for_messages_that_have_none() {
        assert_eq!(method(&json!({ "id": 1, "result": null })), "");
        assert_eq!(method(&json!({ "method": 42 })), "");
        assert_eq!(method(&notification("x/y", json!({}))), "x/y");
    }

    #[test]
    fn a_notification_carries_no_id() {
        assert!(notification("m", json!({})).get("id").is_none());
    }

    #[test]
    fn an_error_response_carries_no_result() {
        let e = error_response(json!(1), -32603, "boom");
        assert_eq!(e["error"]["code"], -32603);
        assert_eq!(e["error"]["message"], "boom");
        assert!(e.get("result").is_none());
        // An error is still a response as far as routing is concerned.
        assert!(is_response(&e));
    }

    #[test]
    fn a_response_with_a_null_result_is_still_a_response() {
        let r = response(json!(1), Value::Null);
        assert!(is_response(&r) && !is_request(&r));
        assert!(r.get("result").is_some());
    }

    #[test]
    fn every_message_declares_jsonrpc_two() {
        for v in [
            request(json!(1), "m", json!({})),
            response(json!(1), json!(1)),
            error_response(json!(1), 1, "e"),
            notification("m", json!({})),
        ] {
            assert_eq!(v["jsonrpc"], "2.0");
        }
    }

    #[test]
    fn internal_ids_are_not_mistaken_for_client_ids() {
        let internal = json!(format!("{INTERNAL}initialize"));
        assert!(is_internal_id(&internal));
        assert_eq!(decode_id(&internal), None);
        assert!(!is_internal_id(&json!(1)));
        assert!(!is_internal_id(&json!("drey")));
    }

    #[test]
    fn encode_produces_parseable_json() {
        let v = request(json!("a"), "m", json!({ "k": [1, 2] }));
        assert_eq!(serde_json::from_slice::<Value>(&encode(&v)).unwrap(), v);
    }

    proptest::proptest! {
        #[test]
        fn numeric_ids_round_trip_for_any_client(client: u64, id: i64) {
            let original = json!(id);
            proptest::prop_assert_eq!(
                decode_id(&encode_id(client, &original)),
                Some((client, original))
            );
        }

        #[test]
        fn string_ids_round_trip_whatever_they_contain(client: u64, id: String) {
            let original = json!(id);
            proptest::prop_assert_eq!(
                decode_id(&encode_id(client, &original)),
                Some((client, original))
            );
        }
    }
}
