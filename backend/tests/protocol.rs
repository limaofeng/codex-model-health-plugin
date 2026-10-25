use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Manifest, Message, PROTOCOL_VERSION, PluginFault,
    Stage,
    call::{
        host::ModelEventBatch,
        model::{ExecutionEvent, WireEvent, WirePayload},
    },
    client::{PluginSession, SessionConfig, read_frame, write_frame},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

struct Peer {
    reader: ReadHalf<DuplexStream>,
    writer: WriteHalf<DuplexStream>,
    task: tokio::task::JoinHandle<()>,
    sequence: u64,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Peer {
    async fn new() -> Self {
        #[cfg(feature = "bundled-engine")]
        let engine = codex_model_health_plugin::engine::Engine::bundled().unwrap();
        #[cfg(not(feature = "bundled-engine"))]
        let engine = {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap();
            codex_model_health_plugin::engine::Engine::command(
                [
                    "docker",
                    "run",
                    "--rm",
                    "-i",
                    "--network",
                    "none",
                    "--read-only",
                    "--tmpfs",
                    "/tmp:rw,noexec,nosuid,size=128m",
                    "-e",
                    "PYTHONDONTWRITEBYTECODE=1",
                    "-v",
                    &format!("{}:/fixture:ro", root.display()),
                    "--entrypoint",
                    "python",
                    "meow-llm-detector:4.5.4",
                    "/fixture/engine/worker.py",
                ]
                .into_iter()
                .map(Into::into)
                .collect(),
            )
        };
        Self::with_engine(engine).await
    }

    async fn with_engine(engine: codex_model_health_plugin::engine::Engine) -> Self {
        let (host, transport) = tokio::io::duplex(2 * 1024 * 1024);
        let (reader, writer) = tokio::io::split(transport);
        let task = tokio::spawn(async move {
            let session = PluginSession::accept(reader, writer, SessionConfig::default())
                .await
                .unwrap();
            session
                .run(codex_model_health_plugin::plugin(engine).unwrap())
                .await
                .unwrap();
        });
        let (reader, writer) = tokio::io::split(host);
        let mut peer = Self {
            reader,
            writer,
            task,
            sequence: 1,
        };
        let manifest = Manifest::from_author_slice(include_bytes!("../../plugin.json")).unwrap();
        peer.send(Frame::control(Message::Hello {
            handshake: Handshake {
                protocol_version: PROTOCOL_VERSION,
                artifact_sha256: "a".repeat(64),
                plugin_id: "local.model-health".into(),
                instance_id: "test".into(),
                generation: 1,
                incarnation: "test".into(),
                configuration: json!({}),
                contributes: manifest.contributes,
            },
        }))
        .await;
        assert!(matches!(
            peer.receive().await.message,
            Message::Ready { .. }
        ));
        peer
    }
    async fn send(&mut self, frame: Frame) {
        write_frame(&mut self.writer, &frame).await.unwrap();
    }
    async fn receive(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(30), read_frame(&mut self.reader))
            .await
            .expect("plugin timeout")
            .unwrap()
    }
    async fn api(
        &mut self,
        path: &str,
        input: Option<Value>,
        host: &mut Host,
    ) -> Result<Value, PluginFault> {
        let id = self.sequence;
        self.sequence += 2;
        self.send(Frame{message:Message::Call{id,method:"management.handle".into(),context:CallContext{call_id:id,instance_id:"test".into(),generation:1,incarnation:"test".into(),stage:Stage::Management,timeout_ms:120000,resource_stream:false,resource_scope_id:"scope".into(),request_id:None,attempt_id:None,account_id:None,credential_revision:None,request:None},params:json!({"method":if input.is_some(){"POST"}else{"GET"},"path":path,"query":"","content_type":input.as_ref().map(|_|"application/json"),"headers":[]})},payload:input.map(|v|serde_json::to_vec(&v).unwrap()).unwrap_or_default()}).await;
        loop {
            let frame = self.receive().await;
            match frame.message {
                Message::Callback {
                    id: cid,
                    parent_id,
                    method,
                    params,
                } => {
                    assert_eq!(parent_id, id);
                    let response = match host.callback(&method, params, frame.payload) {
                        Ok((result, payload)) => Frame {
                            message: Message::Result { id: cid, result },
                            payload,
                        },
                        Err(error) => Frame::control(Message::Error { id: cid, error }),
                    };
                    self.send(response).await;
                }
                Message::Result { id: rid, result } => {
                    assert_eq!(id, rid);
                    assert_eq!(result["content_type"], "application/json");
                    let body: Value = serde_json::from_slice(&frame.payload).unwrap();
                    let status = result["status"].as_u64().unwrap();
                    if status >= 400 {
                        let mut error: PluginFault =
                            serde_json::from_value(body["error"].clone()).unwrap();
                        error.http_status = Some(status as u16);
                        return Err(error);
                    }
                    assert_eq!(status, 200);
                    return Ok(body);
                }
                Message::Error { id: rid, error } => {
                    panic!("application error leaked into protocol: {rid}, {error:?}");
                }
                _ => panic!("unexpected frame"),
            }
        }
    }
    async fn start(&mut self, host: &mut Host) -> Value {
        self.api("api/runs/start",Some(json!({"account_id":"account-b","key_id":"key-b","request_model":"gpt-6-astra","claimed_model":"gpt-6-astra","tier":"low"})),host).await.unwrap()["run"].clone()
    }
}

#[derive(Default)]
struct Host {
    state: BTreeMap<String, (Value, u64)>,
    executions: Vec<Value>,
    bodies: Vec<Value>,
    fail: bool,
    revision_changed: bool,
    refresh_each_request: bool,
    account_patch: Option<Value>,
    repeat_cursor: bool,
    numeric_answer: bool,
    callbacks: Vec<String>,
    http: VecDeque<(u16, Vec<u8>)>,
    http_body: Option<Vec<u8>>,
    http_requests: Vec<Value>,
    http_failure: bool,
}
fn account(id: &str, revision: u64) -> Value {
    json!({"account_id":id,"provider_id":"openai","credential_revision":revision,"name":id,"email":null,"upstream_user_id":format!("user-{id}"),"upstream_account_id":format!("upstream-{id}"),"plan_type":"pro","authentication_kind":"oauth","enabled":true,"credential_state":"ready","has_refresh_token":true,"access_token_expires_at_ms":null,"next_refresh_at_ms":null})
}
impl Host {
    fn account(&self, id: &str) -> Value {
        let revision = if self.revision_changed { 2 } else { 1 }
            + if self.refresh_each_request {
                self.executions.len() as u64
            } else {
                0
            };
        let mut account = account(id, revision);
        if let Some(patch) = &self.account_patch {
            account
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
        }
        account
    }
    fn callback(
        &mut self,
        method: &str,
        params: Value,
        payload: Vec<u8>,
    ) -> Result<(Value, Vec<u8>), PluginFault> {
        self.callbacks.push(method.into());
        let metadata = |value| Ok((value, vec![]));
        match method {
            "host.http.do_stream" => {
                assert!(payload.is_empty());
                assert_eq!(params["method"], "GET");
                assert!(
                    self.http_body.is_none(),
                    "previous HTTP body was not released"
                );
                let headers: Vec<(String, Vec<u8>)> =
                    serde_json::from_value(params["headers"].clone()).unwrap();
                assert_eq!(headers.len(), 3);
                assert_eq!(
                    headers[0],
                    (
                        "User-Agent".into(),
                        format!("codex-model-health/{}", env!("CARGO_PKG_VERSION")).into_bytes()
                    )
                );
                assert_eq!(
                    headers[1],
                    ("Accept".into(), b"application/vnd.github+json".to_vec())
                );
                assert_eq!(
                    headers[2],
                    ("X-GitHub-Api-Version".into(), b"2022-11-28".to_vec())
                );
                self.http_requests.push(params);
                if self.http_failure {
                    return Err(PluginFault::new(ErrorCode::Timeout, "fixture timeout"));
                }
                let (status, body) = self.http.pop_front().expect("unexpected HTTP request");
                self.http_body = Some(body);
                metadata(json!({"status":status,"http_version":"HTTP/1.1","headers":[],"stream":"github","trailers":[]}))
            }
            "host.http.stream_read" => {
                assert!(payload.is_empty());
                assert_eq!(params["stream"], "github");
                let body = self.http_body.as_mut().unwrap();
                if body.is_empty() {
                    self.http_body = None;
                    return metadata(json!({"eof":true}));
                }
                let length = body
                    .len()
                    .min(params["maximum_bytes"].as_u64().unwrap() as usize);
                Ok((json!({"eof":false}), body.drain(..length).collect()))
            }
            "host.http.stream_close" => {
                assert!(payload.is_empty());
                assert_eq!(params["stream"], "github");
                assert!(self.http_body.take().is_some());
                metadata(json!({}))
            }
            "host.auth.list" => {
                assert_eq!(params, json!({}));
                let query: Value = serde_json::from_slice(&payload).unwrap();
                let next = query["cursor"].is_null();
                let body = json!({"accounts":[self.account(if next {"account-a"} else {"account-b"})],"next_cursor":if next || self.repeat_cursor {json!("account-a")}else{Value::Null}});
                Ok((json!({}), serde_json::to_vec(&body).unwrap()))
            }
            "host.auth.get_runtime" => {
                let query: Value = serde_json::from_slice(&payload).unwrap();
                Ok((
                    json!({}),
                    serde_json::to_vec(&self.account(query["account_id"].as_str().unwrap()))
                        .unwrap(),
                ))
            }
            "host.keys.list" => {
                let next = params["cursor"].is_null();
                metadata(
                    json!({"keys":[{"id":if next {"key-a"}else{"key-b"},"name":"test key","enabled":true}],"next_cursor":if next {json!("key-a")}else{Value::Null}}),
                )
            }
            "host.models.list" => {
                assert_eq!(params["client_key_id"], "key-b");
                assert_eq!(params["protocol"], "openai");
                metadata(json!({"models":["gpt-6-astra"]}))
            }
            "host.state.get" => {
                assert_eq!(params["namespace"], "runs");
                let record = self.state.get(params["key"].as_str().unwrap()).map(
                    |(value, version)| json!({"value":value,"version":version,"schema_version":1}),
                );
                metadata(json!({"record":record}))
            }
            "host.state.put" => {
                assert_eq!(params["namespace"], "runs");
                let key = params["key"].as_str().unwrap().to_string();
                let current = self.state.get(&key).map(|(_, v)| *v);
                if current != params["expected_version"].as_u64() {
                    return Err(PluginFault::new(ErrorCode::Conflict, "CAS conflict"));
                }
                let version = current.unwrap_or(0) + 1;
                self.state.insert(key, (params["value"].clone(), version));
                metadata(json!({"version":version}))
            }
            "host.model.execute_stream" => {
                assert_eq!(params["account_id"], "account-b");
                assert_eq!(params["client_key_id"], "key-b");
                assert_eq!(params["provider"], "openai");
                assert_eq!(params["protocol"], "openai");
                assert_eq!(params["model"], "gpt-6-astra");
                assert!(self.state.values().any(|(v, _)| v["inflight"].is_string()
                    && v["attempts"].as_array().unwrap().last().unwrap()["status"] == "pending"));
                self.executions.push(params);
                let body: Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(body["stream"], true);
                assert_eq!(body["store"], false);
                assert!(body["tools"].is_null());
                self.bodies.push(body);
                metadata(
                    json!({"request_id":format!("request-{}",self.executions.len()),"stream":"stream-one"}),
                )
            }
            "host.model.stream_read" => {
                if self.fail {
                    return Err(PluginFault::new(
                        ErrorCode::Upstream,
                        "fixture network error",
                    ));
                }
                let prompt = self.bodies.last().unwrap()["input"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .to_lowercase();
                let answer = if self.numeric_answer {
                    (0..332)
                        .map(|i| (1 + (i * 37 + 13) % 355).to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                } else if prompt.contains("bird") {
                    "robin".into()
                } else if prompt.contains("country") {
                    "canada".into()
                } else {
                    "3".into()
                };
                let data = json!({"type":"response.completed","response":{"id":"response-one","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":answer}]}],"usage":{"input_tokens":10,"output_tokens":1}}});
                let payload = ModelEventBatch {
                    events: vec![ExecutionEvent::wire(WireEvent {
                        protocol: "openai".into(),
                        payload: WirePayload::Json {
                            event: None,
                            data,
                            id: None,
                            retry: None,
                            raw_sse: None,
                        },
                    })],
                }
                .encode()
                .unwrap();
                Ok((json!({"events":1,"end":true}), payload))
            }
            "host.model.stream_close" => metadata(json!({})),
            _ => panic!("unexpected callback: {method}"),
        }
    }
}

#[tokio::test]
async fn exact_account_key_pagination_and_request_evidence() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let boot = peer.api("api/bootstrap", None, &mut host).await.unwrap();
    let accounts = boot["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    for (account, id) in accounts.iter().zip(["account-a", "account-b"]) {
        assert!(account.get("email").is_some());
        assert_eq!(account["id"], id);
        assert_eq!(account["name"], id);
        assert_eq!(account["provider"], "openai");
        assert_eq!(account["plan"], "pro");
    }
    assert!(!boot.to_string().contains("access_token"));
    assert_eq!(boot["sources"].as_array().unwrap().len(), 2);
    assert!(
        !host.callbacks.iter().any(|m| m.starts_with("host.http.")),
        "opening the page must not check GitHub"
    );
    let run = peer.start(&mut host).await;
    assert!(host.executions.is_empty());
    let result = peer
        .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(result["run"]["attempts"][0]["status"], "ok");
    assert_eq!(result["run"]["attempts"][0]["request_id"], "request-1");
    assert_eq!(host.executions.len(), 1);
}

#[tokio::test]
async fn run_account_name_prefers_trimmed_email_and_falls_back_to_name() {
    let mut peer = Peer::new().await;

    let mut host = Host {
        account_patch: Some(json!({"name":"OPENAI账号","email":"  account@example.com  "})),
        ..Host::default()
    };
    let run = peer.start(&mut host).await;
    assert_eq!(run["account_name"], "account@example.com");

    host.account_patch = Some(json!({"name":"  OPENAI账号  ","email":"   "}));
    let run = peer.start(&mut host).await;
    assert_eq!(run["account_name"], "OPENAI账号");

    host.account_patch = Some(json!({"name":"   ","email":null}));
    let run = peer.start(&mut host).await;
    assert_eq!(run["account_name"], "");
}

const MEOW_SHA: &str = "0f31d4a96c9e953277ab8e337d3dbd3ec57b316e";
const TRACE_SHA: &str = "d4131b30243dfa05e70180b5eedde742103f1d73";

fn github_reply(host: &mut Host, status: u16, body: Value) {
    host.http
        .push_back((status, serde_json::to_vec(&body).unwrap()));
}

#[tokio::test]
async fn update_checks_are_per_source_cached_and_do_not_touch_runs_or_accounts() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let run = peer.start(&mut host).await;
    let saved = host.state.clone();
    host.callbacks.clear();
    github_reply(&mut host, 200, json!([{"sha":MEOW_SHA}]));
    let first = peer
        .api(
            "api/upstreams/check",
            Some(json!({"method":"meow"})),
            &mut host,
        )
        .await
        .unwrap();
    assert_eq!(first["check"]["status"], "identical");
    assert_eq!(first["check"]["latest_commit"], MEOW_SHA);
    assert!(first["check"]["checked_at"].as_u64().unwrap() > 0);
    let cached = peer
        .api(
            "api/upstreams/check",
            Some(json!({"method":"meow"})),
            &mut host,
        )
        .await
        .unwrap();
    assert_eq!(first, cached);
    assert_eq!(host.http_requests.len(), 1);
    assert_eq!(
        host.http_requests[0]["url"],
        "https://api.github.com/repos/chen-006/meow-llm-detector/commits?per_page=1"
    );
    let latest = "a".repeat(40);
    github_reply(&mut host, 200, json!([{"sha":latest}]));
    github_reply(
        &mut host,
        200,
        json!({"status":"ahead","ahead_by":2,"behind_by":0,"html_url":"https://untrusted.invalid/"}),
    );
    let trace = peer
        .api(
            "api/upstreams/check",
            Some(json!({"method":"modeltrace"})),
            &mut host,
        )
        .await
        .unwrap();
    assert_eq!(trace["check"]["status"], "ahead");
    assert_eq!(trace["check"]["ahead_by"], 2);
    assert_eq!(
        trace["check"]["compare_url"],
        format!("https://github.com/xqy2006/ModelTrace/compare/{TRACE_SHA}...{latest}")
    );
    assert_eq!(
        host.http_requests[1]["url"],
        "https://api.github.com/repos/xqy2006/ModelTrace/commits?per_page=1"
    );
    assert_eq!(
        host.http_requests[2]["url"],
        format!(
            "https://api.github.com/repos/xqy2006/ModelTrace/compare/{TRACE_SHA}...{latest}?per_page=1&page=2"
        )
    );
    assert!(host.callbacks.iter().all(|m| m.starts_with("host.http.")));
    assert!(host.http_body.is_none() && host.http.is_empty() && host.executions.is_empty());
    assert_eq!(host.state, saved);
    let result = peer
        .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(result["run"]["report"]["valid_samples"], 1);
}

#[tokio::test]
async fn update_checks_distinguish_behind_and_diverged_histories() {
    for (status, ahead, behind) in [("behind", 0, 2), ("diverged", 3, 1)] {
        let mut peer = Peer::new().await;
        let mut host = Host::default();
        github_reply(&mut host, 200, json!([{"sha":"b".repeat(40)}]));
        github_reply(
            &mut host,
            200,
            json!({"status":status,"ahead_by":ahead,"behind_by":behind}),
        );
        let result = peer
            .api(
                "api/upstreams/check",
                Some(json!({"method":"meow"})),
                &mut host,
            )
            .await
            .unwrap();
        assert_eq!(result["check"]["status"], status);
        assert_eq!(result["check"]["ahead_by"], ahead);
        assert_eq!(result["check"]["behind_by"], behind);
        assert!(host.state.is_empty() && host.executions.is_empty());
    }
}

#[tokio::test]
async fn update_errors_close_streams_and_are_not_cached_as_latest() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    for status in [403, 429, 404, 301, 500] {
        github_reply(
            &mut host,
            status,
            json!({"message":"sensitive upstream error not forwarded"}),
        );
        let error = peer
            .api(
                "api/upstreams/check",
                Some(json!({"method":"meow"})),
                &mut host,
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status, Some(502));
        assert!(!error.message.contains("sensitive"));
        assert!(host.http_body.is_none());
    }
    assert_eq!(
        host.callbacks
            .iter()
            .filter(|m| *m == "host.http.stream_close")
            .count(),
        5
    );
    github_reply(&mut host, 200, json!([{"sha":MEOW_SHA}]));
    let result = peer
        .api(
            "api/upstreams/check",
            Some(json!({"method":"meow"})),
            &mut host,
        )
        .await
        .unwrap();
    assert_eq!(result["check"]["status"], "identical");
    assert_eq!(host.http_requests.len(), 6);
    host.http_failure = true;
    let error = peer
        .api(
            "api/upstreams/check",
            Some(json!({"method":"modeltrace"})),
            &mut host,
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status, Some(504));
    assert!(!error.message.contains("fixture"));
    host.http_failure = false;
    github_reply(&mut host, 200, json!([{"sha":TRACE_SHA}]));
    assert!(
        peer.api(
            "api/upstreams/check",
            Some(json!({"method":"modeltrace"})),
            &mut host
        )
        .await
        .is_ok()
    );
    assert!(host.state.is_empty() && host.executions.is_empty());
}

#[tokio::test]
async fn update_checks_reject_invalid_bounded_responses_and_arbitrary_urls() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    for query in [
        json!({}),
        json!({"method":"other"}),
        json!({"method":"meow","url":"https://untrusted.invalid"}),
    ] {
        let error = peer
            .api("api/upstreams/check", Some(query), &mut host)
            .await
            .unwrap_err();
        assert_eq!(error.http_status, Some(400));
    }
    assert!(host.callbacks.is_empty());
    for body in [
        json!([]),
        json!([{"sha":"../bad"}]),
        json!({"message":"wrong shape"}),
    ] {
        github_reply(&mut host, 200, body);
        let error = peer
            .api(
                "api/upstreams/check",
                Some(json!({"method":"meow"})),
                &mut host,
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status, Some(502));
        assert!(host.http_body.is_none());
    }
    host.http.push_back((200, b"not json".to_vec()));
    assert!(
        peer.api(
            "api/upstreams/check",
            Some(json!({"method":"meow"})),
            &mut host
        )
        .await
        .is_err()
    );
    for (status, ahead, behind) in [
        ("identical", 0, 0),
        ("ahead", 0, 0),
        ("behind", 2, 0),
        ("diverged", 0, 1),
        ("unknown", 1, 0),
    ] {
        github_reply(&mut host, 200, json!([{"sha":"c".repeat(40)}]));
        github_reply(
            &mut host,
            200,
            json!({"status":status,"ahead_by":ahead,"behind_by":behind}),
        );
        assert!(
            peer.api(
                "api/upstreams/check",
                Some(json!({"method":"meow"})),
                &mut host
            )
            .await
            .is_err()
        );
    }
    host.http.push_back((200, vec![b' '; 262145]));
    let error = peer
        .api(
            "api/upstreams/check",
            Some(json!({"method":"meow"})),
            &mut host,
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status, Some(503));
    assert_eq!(host.callbacks.last().unwrap(), "host.http.stream_close");
    assert!(host.http_body.is_none() && host.state.is_empty() && host.executions.is_empty());
}

#[tokio::test]
async fn modeltrace_uses_same_host_path_and_resumes_frozen_plan() {
    let mut peer = Peer::new().await;
    let mut host = Host {
        numeric_answer: true,
        refresh_each_request: true,
        ..Host::default()
    };
    let boot = peer.api("api/bootstrap", None, &mut host).await.unwrap();
    assert_eq!(boot["catalog"]["methods"][1]["method"], "modeltrace");
    let run = peer.api("api/runs/start", Some(json!({"method":"modeltrace","account_id":"account-b","key_id":"key-b","request_model":"gpt-6-astra","claimed_model":"unified","tier":"fixed"})), &mut host).await.unwrap()["run"].clone();
    let id = &run["id"];
    assert_eq!(run["plan"]["jobs"].as_array().unwrap().len(), 3);
    assert_eq!(run["report"]["analysis"], Value::Null);
    for completed in 1..=3 {
        let result = peer
            .api("api/runs/step", Some(json!({"id":id})), &mut host)
            .await
            .unwrap();
        assert_eq!(result["run"]["report"]["valid_samples"], completed);
        assert_eq!(result["run"]["plan"], run["plan"]);
        assert_eq!(result["run"]["report"]["method"], "modeltrace");
        assert_eq!(
            result["run"]["attempts"][completed as usize - 1]["status"],
            "ok"
        );
        assert!(serde_json::to_vec(&result["run"]).unwrap().len() < 262144);
        let body = host.bodies.last().unwrap();
        assert!(body["max_output_tokens"].is_null() && body["reasoning"].is_null());
        assert_eq!(
            body["input"][0]["content"],
            run["plan"]["jobs"][completed as usize - 1]["prompt"]
        );
        if completed == 1 {
            peer.api(
                "api/runs/control",
                Some(json!({"id":id,"action":"pause"})),
                &mut host,
            )
            .await
            .unwrap();
            drop(peer);
            peer = Peer::new().await;
            peer.api(
                "api/runs/control",
                Some(json!({"id":id,"action":"resume"})),
                &mut host,
            )
            .await
            .unwrap();
        }
    }
    let saved = peer
        .api("api/runs/get", Some(json!({"id":id})), &mut host)
        .await
        .unwrap();
    assert_eq!(saved["run"]["status"], "completed");
    assert_eq!(saved["run"]["report"]["verdict"], "attributed");
    assert_eq!(
        saved["run"]["report"]["analysis"]["results"]
            .as_array()
            .unwrap()
            .len(),
        17
    );
    let history = peer.api("api/runs", None, &mut host).await.unwrap();
    assert_eq!(history["runs"][0]["method"], "modeltrace");
    assert_eq!(history["runs"][0]["report"], saved["run"]["report"]);
    assert_eq!(host.executions.len(), 3);
    let error = peer
        .api("api/runs/step", Some(json!({"id":id})), &mut host)
        .await
        .unwrap_err();
    assert_eq!(error.http_status, Some(400));
    assert_eq!(host.executions.len(), 3);
}

#[tokio::test]
async fn unknown_method_is_rejected_and_legacy_meow_still_resumes() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let error = peer.api("api/runs/start", Some(json!({"method":"unknown","account_id":"account-b","key_id":"key-b","request_model":"gpt-6-astra","claimed_model":"gpt-6-astra","tier":"low"})), &mut host).await.unwrap_err();
    assert_eq!(error.http_status, Some(400));
    assert!(host.state.is_empty() && host.executions.is_empty());
    let run = peer.start(&mut host).await;
    host.state.values_mut().next().unwrap().0["plan"]
        .as_object_mut()
        .unwrap()
        .remove("method");
    let history = peer.api("api/runs", None, &mut host).await.unwrap();
    assert_eq!(history["runs"][0]["method"], "meow");
    let result = peer
        .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(result["run"]["report"]["valid_samples"], 1);
    assert!(result["run"]["report"]["matches"].is_object());
}
#[tokio::test]
async fn network_errors_pause_without_calling_them_model_mismatch() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let run = peer.start(&mut host).await;
    host.fail = true;
    let result = peer
        .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(result["run"]["status"], "paused");
    assert_eq!(result["run"]["report"]["verdict"], "insufficient");
    assert_eq!(result["run"]["report"]["valid_samples"], 0);
    assert_eq!(result["run"]["attempts"][0]["request_id"], "request-1");
    assert_eq!(host.executions.len(), 1);
    assert!(
        peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
            .await
            .is_err()
    );
    assert_eq!(host.executions.len(), 1);
}
#[tokio::test]
async fn pause_resume_and_persist_across_real_sessions() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let run = peer.start(&mut host).await;
    peer.api(
        "api/runs/control",
        Some(json!({"id":run["id"],"action":"pause"})),
        &mut host,
    )
    .await
    .unwrap();
    drop(peer);
    let mut peer = Peer::new().await;
    let saved = peer
        .api("api/runs/get", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(saved["run"]["status"], "paused");
    assert!(host.executions.is_empty());
    peer.api(
        "api/runs/control",
        Some(json!({"id":run["id"],"action":"resume"})),
        &mut host,
    )
    .await
    .unwrap();
    peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(host.executions.len(), 1);
    host.revision_changed = true;
    drop(peer);
    let mut peer = Peer::new().await;
    peer.api(
        "api/runs/control",
        Some(json!({"id":run["id"],"action":"resume"})),
        &mut host,
    )
    .await
    .unwrap();
    peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(host.executions.len(), 2);
    peer.api(
        "api/runs/control",
        Some(json!({"id":run["id"],"action":"stop"})),
        &mut host,
    )
    .await
    .unwrap();
    let stopped = host.state.clone();
    assert!(
        peer.api(
            "api/runs/control",
            Some(json!({"id":run["id"],"action":"resume"})),
            &mut host
        )
        .await
        .is_err()
    );
    assert_eq!(host.state, stopped);
}
#[tokio::test]
async fn changed_identity_and_repeated_cursor_fail_before_inference() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let run = peer.start(&mut host).await;
    host.account_patch = Some(json!({"upstream_account_id":"different-account"}));
    assert!(
        peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
            .await
            .is_err()
    );
    assert!(host.executions.is_empty());
    host.repeat_cursor = true;
    assert!(peer.api("api/bootstrap", None, &mut host).await.is_err());
}
#[tokio::test]
async fn malformed_start_does_not_mutate_host() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let error = peer
        .api(
            "api/runs/start",
            Some(json!({"account_id":"account-b","api_key":"should-not-be-accepted"})),
            &mut host,
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status, Some(400));
    assert_eq!(error.message, "请求参数格式错误");
    assert!(host.state.is_empty());
    assert!(host.executions.is_empty());
}

#[tokio::test]
async fn bootstrap_engine_failure_is_an_actionable_http_error_without_inference() {
    let missing = std::env::temp_dir().join(format!(
        "missing-model-health-engine-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut peer = Peer::with_engine(codex_model_health_plugin::engine::Engine::command(vec![
        missing.into_os_string(),
    ]))
    .await;
    let mut host = Host::default();
    for _ in 0..2 {
        let error = peer
            .api("api/bootstrap", None, &mut host)
            .await
            .unwrap_err();
        assert_eq!(error.http_status, Some(500));
        assert_eq!(error.code, ErrorCode::Fault);
        assert!(error.message.starts_with("检测引擎"));
        assert!(!error.message.contains("副作用"));
    }
    assert!(host.state.is_empty());
    assert!(host.executions.is_empty());
}

#[tokio::test]
async fn full_plan_finishes_and_report_survives_reopen() {
    let mut peer = Peer::new().await;
    let mut host = Host {
        refresh_each_request: true,
        ..Host::default()
    };
    let run = peer.start(&mut host).await;
    let mut latest = Value::Null;
    for _ in 0..36 {
        latest = peer
            .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
            .await
            .unwrap()["run"]
            .clone();
    }
    assert_eq!(latest["status"], "completed");
    assert_eq!(latest["report"]["valid_samples"], 36);
    assert_eq!(host.executions.len(), 36);
    assert_eq!(host.account("account-b")["credential_revision"], 37);
    assert_eq!(latest["credential_revision"], 36);
    assert_eq!(latest["account_identity"], run["account_identity"]);
    assert!(serde_json::to_vec(&latest).unwrap().len() < 262144);
    let original = latest.clone();
    assert!(
        peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
            .await
            .is_err()
    );
    drop(peer);
    let mut reopened = Peer::new().await;
    let restored = reopened
        .api("api/runs/get", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(restored["run"], original);
    assert_eq!(host.executions.len(), 36);
}

#[tokio::test]
async fn account_guard_rejects_disabled_or_replaced_identity_without_mutating_state() {
    for patch in [
        json!({"enabled":false}),
        json!({"account_id":"another-local-account"}),
        json!({"provider_id":"xai"}),
        json!({"authentication_kind":"api_key"}),
        json!({"upstream_user_id":"another-user"}),
        json!({"upstream_account_id":"another-workspace"}),
    ] {
        let mut peer = Peer::new().await;
        let mut host = Host::default();
        let run = peer.start(&mut host).await;
        host.account_patch = Some(patch);
        let saved = host.state.clone();
        for (path, input) in [
            ("api/runs/step", json!({"id":run["id"]})),
            (
                "api/runs/control",
                json!({"id":run["id"],"action":"resume"}),
            ),
        ] {
            let error = peer.api(path, Some(input), &mut host).await.unwrap_err();
            assert_eq!(error.http_status, Some(400));
            assert!(error.message.contains("身份已改变") || error.message.contains("已停用"));
            assert_eq!(host.state, saved);
            assert!(host.executions.is_empty());
        }
    }
}

#[tokio::test]
async fn legacy_runs_require_attributable_history_before_pinning_current_identity() {
    for (has_sample, revision_changed) in [(false, true), (true, false), (true, true)] {
        let mut peer = Peer::new().await;
        let mut host = Host::default();
        let run = peer.start(&mut host).await;
        if has_sample {
            peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
                .await
                .unwrap();
        }
        host.state
            .values_mut()
            .next()
            .unwrap()
            .0
            .as_object_mut()
            .unwrap()
            .remove("account_identity");
        host.revision_changed = revision_changed;
        let saved = host.state.clone();
        let result = peer
            .api(
                "api/runs/control",
                Some(json!({"id":run["id"],"action":"resume"})),
                &mut host,
            )
            .await;
        if has_sample && revision_changed {
            assert!(
                result
                    .unwrap_err()
                    .message
                    .contains("旧检测缺少账号身份快照")
            );
            assert_eq!(host.state, saved);
            assert_eq!(host.executions.len(), 1);
            let history = peer
                .api("api/runs/get", Some(json!({"id":run["id"]})), &mut host)
                .await
                .unwrap();
            assert_eq!(
                history["run"]["report"],
                saved.values().next().unwrap().0["report"]
            );
            peer.api(
                "api/runs/control",
                Some(json!({"id":run["id"],"action":"stop"})),
                &mut host,
            )
            .await
            .unwrap();
        } else {
            assert_eq!(
                result.unwrap()["run"]["account_identity"],
                run["account_identity"]
            );
            peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
                .await
                .unwrap();
            assert_eq!(host.executions.len(), if has_sample { 2 } else { 1 });
        }
    }
}

#[tokio::test]
async fn missing_upstream_identity_does_not_silently_accept_credential_replacement() {
    let mut peer = Peer::new().await;
    let mut host = Host {
        account_patch: Some(json!({"upstream_user_id":null,"upstream_account_id":null})),
        ..Host::default()
    };
    let run = peer.start(&mut host).await;
    peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    host.revision_changed = true;
    let saved = host.state.clone();
    let error = peer
        .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap_err();
    assert!(error.message.contains("缺少稳定的上游身份"));
    assert_eq!(host.state, saved);
    assert_eq!(host.executions.len(), 1);
}

#[tokio::test]
async fn no_samples_can_rebase_revision_without_rejecting_the_next_unchanged_step() {
    let mut peer = Peer::new().await;
    let mut host = Host {
        account_patch: Some(json!({"upstream_user_id":null,"upstream_account_id":null})),
        ..Host::default()
    };
    let run = peer.start(&mut host).await;
    host.revision_changed = true;
    for _ in 0..2 {
        let result = peer
            .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
            .await
            .unwrap();
        assert_eq!(result["run"]["credential_revision"], 2);
    }
    assert_eq!(host.executions.len(), 2);
}

#[tokio::test]
async fn metadata_and_token_refresh_do_not_change_account_identity() {
    let mut peer = Peer::new().await;
    let mut host = Host::default();
    let run = peer.start(&mut host).await;
    peer.api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    host.revision_changed = true;
    host.account_patch = Some(
        json!({"name":"renamed","email":"fixture@example.invalid","plan_type":"team","access_token_expires_at_ms":2000000000000_i64,"next_refresh_at_ms":1900000000000_i64}),
    );
    let result = peer
        .api("api/runs/step", Some(json!({"id":run["id"]})), &mut host)
        .await
        .unwrap();
    assert_eq!(result["run"]["account_identity"], run["account_identity"]);
    assert_eq!(result["run"]["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(host.executions.len(), 2);
}
