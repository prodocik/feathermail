use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use feathermail_core::Core;
use feathermail_mcp::{call_tool, tool_definitions, Access, PermissionLevel, PROTOCOL_VERSION};
use serde_json::{json, Value};

fn main() {
    if let Err(message) = run() {
        eprintln!("Feather Mail MCP stopped: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut core = Core::open_default().map_err(|e| e.message)?;
    let access = access_from_env();
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|_| "Could not read MCP input.".to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_json(&mut stdout, &rpc_error(Value::Null, -32700, "Parse error"))?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = handle(&mut core, &access, method, &params);
        if let Some(id) = id {
            let response = match result {
                Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                Err((code, message, data)) => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}})
                }
            };
            write_json(&mut stdout, &response)?;
        }
    }
    Ok(())
}

type RpcResult = Result<Value, (i64, String, Value)>;

/// Accept the only resource URI that Feather Mail advertises.  This is a
/// byte-exact identifier, not a URL to normalize or decode: account ids use
/// the same ASCII alphabet as the local Core account-id generator.
fn canonical_mail_account_id(uri: &str) -> Option<&str> {
    let account_id = uri.strip_prefix("mail://")?.strip_suffix('/')?;
    (!account_id.is_empty()
        && account_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
    .then_some(account_id)
}

fn handle(core: &mut Core, access: &Access, method: &str, params: &Value) -> RpcResult {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}, "resources": {"subscribe": false, "listChanged": false}},
            "serverInfo": {"name":"feathermail","version":env!("CARGO_PKG_VERSION")}
        })),
        "notifications/initialized" | "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools":tool_definitions()})),
        "resources/list" => {
            // Route resources through the exact same live Core policy as
            // tools.  This is deliberately not a startup-time snapshot: a
            // user who turns MCP off while stdio remains running immediately
            // stops metadata exposure too.
            let accounts = call_with_confirmation(core, access, "list_accounts", &json!({}))
                .map_err(tool_rpc)?;
            let resources = accounts["accounts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|account| {
                    let id = account.get("id")?.as_str()?;
                    let name = account.get("name")?.as_str()?;
                    Some(json!({"uri":format!("mail://{id}/"),"name":name,"description":"Feather Mail local account","mimeType":"application/json"}))
                })
                .collect::<Vec<_>>();
            Ok(json!({"resources":resources}))
        }
        "resources/read" => {
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| (-32602, "Invalid mail resource URI".into(), Value::Null))?;
            let account = canonical_mail_account_id(uri)
                .ok_or_else(|| (-32602, "Invalid mail resource URI".into(), Value::Null))?;
            let data =
                call_with_confirmation(core, access, "get_account", &json!({"account_id":account}))
                    .map_err(tool_rpc)?;
            Ok(
                json!({"contents":[{"uri":uri,"mimeType":"application/json","text":data.to_string()}]}),
            )
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| (-32602, "Tool name is required".into(), Value::Null))?;
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_with_confirmation(core, access, name, &args) {
                Ok(value) => Ok(
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false}),
                ),
                Err(error) => Ok(
                    json!({"content":[{"type":"text","text":error.message}],"structuredContent":{"code":error.code},"isError":true}),
                ),
            }
        }
        _ => Err((-32601, "Method not found".into(), Value::Null)),
    }
}

/// Only the stdio process waits.  It never holds a transaction or GTK lock:
/// every retry re-checks the persisted on/off setting and consumes an
/// Allow-once decision atomically in Core.  With no running GTK window the
/// durable request simply expires and this remains `PERMISSION_DENIED`.
fn call_with_confirmation(
    core: &mut Core,
    access: &Access,
    name: &str,
    args: &Value,
) -> Result<Value, feathermail_mcp::McpError> {
    call_with_confirmation_until(
        core,
        access,
        name,
        args,
        Instant::now() + Duration::from_secs(125),
    )
}

/// Separated only so the headless timeout contract is testable without
/// waiting for the real 125-second bound.
fn call_with_confirmation_until(
    core: &mut Core,
    access: &Access,
    name: &str,
    args: &Value,
    deadline: Instant,
) -> Result<Value, feathermail_mcp::McpError> {
    let first = call_tool(core, access, name, args);
    let Err(first_error) = first else {
        return first;
    };
    let Some(request_id) = first_error.pending_confirmation() else {
        return Err(first_error);
    };
    loop {
        if Instant::now() >= deadline {
            return Err(first_error);
        }
        std::thread::sleep(Duration::from_millis(200));
        match call_tool(core, access, name, args) {
            Err(error) if error.pending_confirmation() == Some(request_id) => continue,
            result => return result,
        }
    }
}

fn access_from_env() -> Access {
    let accounts = std::env::var("FEATHERMAIL_MCP_ACCOUNTS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    Access {
        client_id: std::env::var("FEATHERMAIL_MCP_CLIENT_ID").unwrap_or_else(|_| "stdio".into()),
        ceiling: PermissionLevel::parse(
            &std::env::var("FEATHERMAIL_MCP_PERMISSION").unwrap_or_else(|_| "draft".into()),
        ),
        accounts,
        attachment_root: std::env::var_os("FEATHERMAIL_MCP_ATTACHMENT_ROOT").map(PathBuf::from),
    }
}

fn tool_rpc(error: feathermail_mcp::McpError) -> (i64, String, Value) {
    (-32000, error.message, json!({"code":error.code}))
}
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn write_json(out: &mut impl Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *out, value)
        .map_err(|_| "Could not encode MCP response.".to_string())?;
    out.write_all(b"\n")
        .and_then(|()| out.flush())
        .map_err(|_| "Could not write MCP response.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::{ConnectError, ConnectOk, MailConnector, MailSecurity, MailboxForm};

    struct LocalProbe;

    impl MailConnector for LocalProbe {
        fn probe(&self, _form: &MailboxForm, _password: &str) -> Result<ConnectOk, ConnectError> {
            Ok(ConnectOk {
                capabilities: Vec::new(),
            })
        }
    }

    fn mailbox_form(email: &str) -> MailboxForm {
        MailboxForm {
            email: email.into(),
            imap_host: "imap.example.test".into(),
            imap_port: 993,
            imap_security: MailSecurity::Ssl,
            smtp_host: "smtp.example.test".into(),
            smtp_port: 465,
            smtp_security: MailSecurity::Ssl,
        }
    }

    fn enabled_core_with_two_accounts() -> (
        Core,
        feathermail_core::AccountId,
        feathermail_core::AccountId,
    ) {
        let mut core = Core::memory().unwrap();
        let first = core
            .add_account(&mailbox_form("first@example.test"), "x", &LocalProbe)
            .unwrap();
        let second = core
            .add_account(&mailbox_form("second@example.test"), "x", &LocalProbe)
            .unwrap();
        core.set_mcp_enabled(1, true).unwrap();
        (core, first, second)
    }
    #[test]
    fn initialize_works_even_when_tools_are_disabled() {
        let mut core = Core::memory().unwrap();
        let result = handle(&mut core, &Access::default(), "initialize", &json!({})).unwrap();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
    }
    #[test]
    fn tool_call_is_disabled_by_default() {
        let mut core = Core::memory().unwrap();
        let response = handle(
            &mut core,
            &Access::default(),
            "tools/call",
            &json!({"name":"list_accounts"}),
        )
        .unwrap();
        assert!(response["isError"].as_bool().unwrap());
        assert_eq!(response["structuredContent"]["code"], "PERMISSION_DENIED");
    }

    /// T-065c: stdio owns two error envelopes. Neither may reflect arbitrary
    /// tool/resource input or let it cross into the metadata-only audit row.
    #[test]
    fn stdio_error_envelopes_and_audit_do_not_echo_untrusted_inputs() {
        let mut core = Core::memory().unwrap();
        core.set_mcp_enabled(1, true).unwrap();
        let queue_before = core.queue_counts().unwrap();
        let command_name = "run_shell_command";
        let command_arg = "inspect";
        let tool_error = handle(
            &mut core,
            &Access::default(),
            "tools/call",
            &json!({
                "name":command_name,
                "arguments":{"command":command_arg},
            }),
        )
        .unwrap();
        assert_eq!(tool_error["isError"], true);
        assert_eq!(tool_error["structuredContent"]["code"], "INVALID_ARGUMENT");
        let tool_rendered = serde_json::to_string(&tool_error).unwrap();
        for raw in [command_name, command_arg] {
            assert!(
                !tool_rendered.contains(raw),
                "tools/call error reflected untrusted input"
            );
        }
        assert_eq!(core.queue_counts().unwrap(), queue_before);

        let unknown_uri = "mail://unknown-local-resource/";
        let resource_error = handle(
            &mut core,
            &Access::default(),
            "resources/read",
            &json!({"uri":unknown_uri}),
        )
        .unwrap_err();
        assert_eq!(resource_error.0, -32000);
        assert_eq!(resource_error.2["code"], "ACCOUNT_NOT_FOUND");
        assert!(!resource_error.1.contains(unknown_uri));
        assert!(!serde_json::to_string(&resource_error.2)
            .unwrap()
            .contains(unknown_uri));
        assert_eq!(core.queue_counts().unwrap(), queue_before);

        let malformed_uri = "mail://unknown-local-resource/path/";
        let malformed_resource_error = handle(
            &mut core,
            &Access::default(),
            "resources/read",
            &json!({"uri":malformed_uri}),
        )
        .unwrap_err();
        assert_eq!(malformed_resource_error.0, -32602);
        assert_eq!(malformed_resource_error.1, "Invalid mail resource URI");
        assert_eq!(malformed_resource_error.2, Value::Null);
        assert!(!serde_json::to_string(&malformed_resource_error)
            .unwrap()
            .contains(malformed_uri));
        assert_eq!(core.queue_counts().unwrap(), queue_before);

        let audit = core.list_mcp_audit(2).unwrap();
        assert_eq!(audit[0].client_id, "stdio");
        assert_eq!(audit[0].tool, "get_account");
        assert_eq!(audit[0].outcome, "denied_or_error");
        assert_eq!(audit[1].client_id, "stdio");
        assert_eq!(audit[1].tool, "unknown");
        assert_eq!(audit[1].outcome, "denied_or_error");
        let audit_rendered = format!("{audit:?}");
        for raw in [command_name, command_arg, unknown_uri] {
            assert!(
                !audit_rendered.contains(raw),
                "audit persisted untrusted input"
            );
        }
    }

    #[test]
    fn resources_read_accepts_only_canonical_mail_account_uris() {
        let (mut core, first, _) = enabled_core_with_two_accounts();
        let access = Access::default();
        let listed = handle(&mut core, &access, "resources/list", &json!({})).unwrap();
        let uri = listed["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|resource| {
                (resource["uri"].as_str() == Some(&format!("mail://{}/", first.as_str())))
                    .then(|| resource["uri"].as_str().unwrap())
            })
            .unwrap();
        let allowed = handle(&mut core, &access, "resources/read", &json!({"uri":uri})).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(allowed["contents"][0]["text"].as_str().unwrap())
                .unwrap()["id"],
            first.as_str()
        );

        let queue_before = core.queue_counts().unwrap();
        let audit_before = core.list_mcp_audit(100).unwrap();
        for malformed in [
            "",
            "mail://",
            "mail:///",
            "https://first/",
            "mail://first",
            "mail://first//",
            "mail://first/path/",
            "mail://first/?query=1",
            "mail://first/#fragment",
            "mail://user@first/",
            "mail://first:993/",
            "mail://first%2Fother/",
        ] {
            let error = handle(
                &mut core,
                &access,
                "resources/read",
                &json!({"uri":malformed}),
            )
            .unwrap_err();
            assert_eq!(error.0, -32602, "{malformed}");
            assert_eq!(error.1, "Invalid mail resource URI", "{malformed}");
            assert_eq!(error.2, Value::Null, "{malformed}");
            if !malformed.is_empty() {
                assert!(
                    !serde_json::to_string(&error).unwrap().contains(malformed),
                    "resource error reflected malformed URI"
                );
            }
        }
        assert_eq!(core.queue_counts().unwrap(), queue_before);
        assert_eq!(core.list_mcp_audit(100).unwrap(), audit_before);
    }

    #[test]
    fn disabled_mcp_does_not_expose_resources() {
        let mut core = Core::memory().unwrap();
        for method in ["resources/list", "resources/read"] {
            let params = if method == "resources/read" {
                json!({"uri":"mail://any/"})
            } else {
                json!({})
            };
            let error = handle(&mut core, &Access::default(), method, &params).unwrap_err();
            assert_eq!(error.2["code"], "PERMISSION_DENIED");
        }
    }

    #[test]
    fn resource_reads_and_normal_account_reads_cannot_cross_the_account_ceiling() {
        let (mut core, first, second) = enabled_core_with_two_accounts();
        let access = Access {
            accounts: HashSet::from([first.as_str().to_string()]),
            ..Access::default()
        };

        let listed = handle(&mut core, &access, "resources/list", &json!({})).unwrap();
        assert_eq!(
            listed["resources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|resource| resource["uri"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            vec![format!("mail://{}/", first.as_str())]
        );

        let allowed = handle(
            &mut core,
            &access,
            "resources/read",
            &json!({"uri":format!("mail://{}/", first.as_str())}),
        )
        .unwrap();
        let account: Value =
            serde_json::from_str(allowed["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(account["id"], first.as_str());

        let resource_error = handle(
            &mut core,
            &access,
            "resources/read",
            &json!({"uri":format!("mail://{}/", second.as_str())}),
        )
        .unwrap_err();
        assert_eq!(resource_error.2["code"], "PERMISSION_DENIED");

        let tool_error = handle(
            &mut core,
            &access,
            "tools/call",
            &json!({"name":"get_account","arguments":{"account_id":second.as_str()}}),
        )
        .unwrap();
        assert!(tool_error["isError"].as_bool().unwrap());
        assert_eq!(tool_error["structuredContent"]["code"], "PERMISSION_DENIED");
    }

    #[test]
    fn resources_recheck_the_live_core_switch_after_stdio_starts() {
        let (mut core, first, _) = enabled_core_with_two_accounts();
        let access = Access::default();
        assert!(handle(&mut core, &access, "resources/list", &json!({})).is_ok());
        core.set_mcp_enabled(2, false).unwrap();

        for (method, params) in [
            ("resources/list", json!({})),
            (
                "resources/read",
                json!({"uri":format!("mail://{}/", first.as_str())}),
            ),
        ] {
            let error = handle(&mut core, &access, method, &params).unwrap_err();
            assert_eq!(error.2["code"], "PERMISSION_DENIED");
        }
    }

    #[test]
    fn headless_confirmation_timeout_fails_closed_without_a_retry_loop() {
        let mut core = Core::memory().unwrap();
        core.set_mcp_enabled(1, true).unwrap();
        assert!(core
            .set_mcp_client_permission_level("stdio", PermissionLevel::Full)
            .unwrap());
        // Seed the opaque Core request without an account row: the timeout
        // test exercises stdio waiting, not a mail lookup or mutation.
        let pending = core
            .authorize_mcp_action(
                "stdio",
                PermissionLevel::Full,
                "delete_message",
                PermissionLevel::Full,
                true,
                None,
                Some("thread1"),
                "delete_message:acc1:thread1",
            )
            .unwrap();
        assert!(matches!(
            pending,
            feathermail_core::McpAuthorization::NeedsConfirmation(_)
        ));
        let error = call_with_confirmation_until(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Full,
                ..Access::default()
            },
            "delete_message",
            &json!({"account_id":"acc1","thread_id":"thread1"}),
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert!(error.pending_confirmation().is_some());
    }
}
