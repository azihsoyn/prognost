//! The seam between a frontend and a Hono backend. The frontend calls
//! the API through Hono's RPC client — `apiClient.api.v1['order-items'][':orderId'].$get(...)`
//! — and the backend registers the handler as `.get('/:orderId', …)` on
//! a router that `index.ts` mounts with `.route('/api/v1/order-items', OrderItemController)`.
//! Both sides spell out the same path, so the call and the handler can
//! be joined by method and path with no type information at all.

use std::sync::LazyLock;

use regex::Regex;

/// `GET` + `["api", "v1", "order-items", ":orderId"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcCall {
    pub method: String,
    pub segments: Vec<String>,
}

static RPC_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?x)
        \b(?P<client>[A-Za-z_$][\w$]*)
        (?P<chain>(?:\s*\.\s*[A-Za-z_$][\w$]*|\s*\[\s*['"][^'"\n]+['"]\s*\])+)
        \s*\.\s*\$(?P<method>get|post|put|patch|delete)\s*\(
        "#,
    )
    .expect("static regex")
});
static SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.\s*([A-Za-z_$][\w$]*)|\[\s*['"]([^'"\n]+)['"]\s*\]"#).expect("static regex")
});
static MOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.route\(\s*['"]([^'"\n]*)['"]\s*,\s*([A-Za-z_$][\w$]*)\s*\)"#)
        .expect("static regex")
});

fn segments_of(chain: &str) -> Vec<String> {
    SEGMENT
        .captures_iter(chain)
        .filter_map(|c| {
            c.get(1)
                .or_else(|| c.get(2))
                .map(|m| m.as_str().to_string())
        })
        .collect()
}

/// A recorded call text that is a Hono RPC call, parsed. The call text
/// as the extractor writes it has no trailing `(`, so one is appended.
pub fn rpc_call(call_text: &str) -> Option<RpcCall> {
    let probe = format!("{call_text}(");
    let c = RPC_CALL.captures(&probe)?;
    if c.get(0)?.start() != 0 {
        return None;
    }
    Some(RpcCall {
        method: c["method"].to_uppercase(),
        segments: segments_of(&c["chain"]),
    })
}

/// Every RPC call in a source file, with the line it starts on.
pub fn rpc_calls_in(source: &str) -> Vec<(RpcCall, u32)> {
    if !source.contains(".$") {
        return Vec::new();
    }
    RPC_CALL
        .captures_iter(source)
        .map(|c| {
            let start = c.get(0).unwrap().start();
            let line = source[..start].matches('\n').count() as u32 + 1;
            (
                RpcCall {
                    method: c["method"].to_uppercase(),
                    segments: segments_of(&c["chain"]),
                },
                line,
            )
        })
        .collect()
}

/// `.route('/prefix', Binding)` statements in a file: (prefix, binding).
pub fn mounts_in(source: &str) -> Vec<(String, String)> {
    MOUNT
        .captures_iter(source)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// `/api/v1/orders/:orderId{[0-9]+}` → `["api", "v1", "orders", ":orderId"]`.
pub fn path_segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match s.find('{') {
            Some(i) if s.starts_with(':') => s[..i].to_string(),
            _ => s.to_string(),
        })
        .collect()
}

/// Two paths name the same route when their segments line up, with any
/// parameter matching any parameter (`:orderId` vs `:id`).
pub fn same_route(a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x == y || (x.starts_with(':') && y.starts_with(':')) || x == "*" || y == "*"
        })
}

/// `"DELETE /:orderId"` (the extractor's route label) → (method, path).
pub fn split_route_label(label: &str) -> Option<(&str, &str)> {
    label.split_once(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_rpc_call_text() {
        let c = rpc_call("apiClient.api.v1['order-items'][':orderId'].$get").unwrap();
        assert_eq!(c.method, "GET");
        assert_eq!(c.segments, ["api", "v1", "order-items", ":orderId"]);
        assert!(rpc_call("db.folderDomain.find").is_none());
        assert!(rpc_call("c.json").is_none());
    }

    #[test]
    fn finds_calls_across_lines_with_their_line() {
        let src = "const a = 1;\nawait apiClient.api.v1['order-items'][':orderId'][\n  ':itemId'\n].$put({\n});\nx.$get(";
        let calls = rpc_calls_in(src);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, 2);
        assert_eq!(calls[0].0.method, "PUT");
        assert_eq!(
            calls[0].0.segments,
            ["api", "v1", "order-items", ":orderId", ":itemId"]
        );
    }

    #[test]
    fn mounts_and_paths() {
        let m = mounts_in(
            "app.route('/health', HealthCheckController);\nconst x = new Hono()\n  .route('/api/v1/orders', OrderController)\n  .route('/api/v1', NotificationSettingController);",
        );
        assert_eq!(m.len(), 3);
        assert_eq!(
            m[1],
            ("/api/v1/orders".to_string(), "OrderController".to_string())
        );
        assert_eq!(
            path_segments("/api/v1/orders/:orderId{[0-9]+}/"),
            ["api", "v1", "orders", ":orderId"]
        );
        assert!(same_route(
            &path_segments("/api/v1/orders/:orderId"),
            &path_segments("/api/v1/orders/:id")
        ));
        assert!(!same_route(
            &path_segments("/api/v1/orders/:orderId"),
            &path_segments("/api/v1/orders")
        ));
        assert_eq!(
            split_route_label("DELETE /:orderId"),
            Some(("DELETE", "/:orderId"))
        );
    }
}
