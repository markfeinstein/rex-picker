#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use rex_picker::{
    PickerResult, PickerSelection,
    adapters::pi_model::{PiModelAdapter, PiModelClient, PiModelScope},
};
use serde_json::{Value, json};

const SCOPED_LIST: &str = concat!(
    "ok;protocol=2;cap_scope=1;cap_refresh=1;scope=scoped;scoped=1;thinking=high;context=72K;overflow=0;tiered=0;idle=1;refresh=current\t\t   MODEL  PROVIDER\n",
    "[\"github-copilot\",\"gpt-5.6-sol\"]\t{\"action\":\"select\",\"provider\":\"github-copilot\",\"modelId\":\"gpt-5.6-sol\",\"thinkingLevel\":\"high\"}\t✓  Sol 5.6  GitHub\n",
    "[\"anthropic\",\"claude-opus-4-6\"]\t{\"action\":\"select\",\"provider\":\"anthropic\",\"modelId\":\"claude-opus-4-6\"}\t   Opus 4.6  Anthropic\n",
);

const ALL_LIST: &str = concat!(
    "ok;protocol=2;cap_scope=1;cap_refresh=1;scope=all;scoped=1;thinking=high;context=72K;overflow=0;tiered=0;idle=1;refresh=ok\t\t   MODEL  PROVIDER\n",
    "[\"google\",\"gemini-3-flash\"]\t{\"action\":\"select\",\"provider\":\"google\",\"modelId\":\"gemini-3-flash\"}\t   Gemini 3 Flash  Google\n",
);

struct MockBridge {
    requests: Arc<Mutex<Vec<Value>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockBridge {
    fn start(socket: &Path, responses: Vec<&'static str>) -> Self {
        let listener = UnixListener::bind(socket).expect("bind mock Pi bridge");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let thread = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept picker connection");
                let request = read_request(&mut stream);
                captured.lock().expect("request lock").push(request);
                stream
                    .write_all(response.as_bytes())
                    .expect("write picker response");
            }
        });
        Self {
            requests,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Drop for MockBridge {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("mock Pi bridge thread");
        }
    }
}

fn read_request(stream: &mut UnixStream) -> Value {
    let mut request = String::new();
    BufReader::new(stream)
        .read_line(&mut request)
        .expect("read picker request");
    serde_json::from_str(request.trim_end()).expect("parse picker request")
}

fn client(socket: &Path) -> PiModelClient {
    PiModelClient::new(socket).with_timeouts(Duration::from_secs(1), Duration::from_secs(1))
}

#[test]
fn loads_models_and_submits_the_structured_selection() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let socket = directory.path().join("picker.sock");
    let bridge = MockBridge::start(
        &socket,
        vec![SCOPED_LIST, "ok\tgithub-copilot/gpt-5.6-sol\n"],
    );
    let mut adapter = PiModelAdapter::new(client(&socket), None);

    let document = adapter.load(false).expect("load models");
    assert_eq!(document.items.len(), 2);
    assert_eq!(
        document.options.header.as_deref(),
        Some("   MODEL  PROVIDER")
    );
    assert_eq!(document.items[0].label, "✓  Sol 5.6  GitHub");

    let selected = PickerSelection {
        id: document.items[0].id.clone(),
        label: document.items[0].label.clone(),
        metadata: document.items[0].metadata.clone(),
    };
    assert!(
        adapter
            .handle_result(&PickerResult {
                action: "accept".to_owned(),
                selected: vec![selected],
            })
            .expect("select model")
            .is_none()
    );
    let requests = bridge.requests();
    drop(adapter);
    drop(bridge);
    assert_eq!(
        requests,
        vec![
            json!({ "action": "list" }),
            json!({
                "action": "select",
                "provider": "github-copilot",
                "modelId": "gpt-5.6-sol",
                "thinkingLevel": "high"
            }),
        ]
    );
}

#[test]
fn scope_and_refresh_actions_reload_without_closing() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let socket = directory.path().join("picker.sock");
    let bridge = MockBridge::start(&socket, vec![SCOPED_LIST, ALL_LIST, ALL_LIST]);
    let mut adapter = PiModelAdapter::new(client(&socket), Some(PiModelScope::Scoped));

    adapter.load(false).expect("initial list");
    let all_document = adapter
        .handle_result(&PickerResult {
            action: "toggle-scope".to_owned(),
            selected: Vec::new(),
        })
        .expect("toggle scope")
        .expect("replacement document");
    assert!(
        all_document
            .options
            .footer
            .as_deref()
            .unwrap_or_default()
            .contains("all")
    );

    let refreshed = adapter
        .handle_result(&PickerResult {
            action: "refresh".to_owned(),
            selected: Vec::new(),
        })
        .expect("refresh")
        .expect("replacement document");
    assert_eq!(refreshed.items[0].label, "   Gemini 3 Flash  Google");

    let requests = bridge.requests();
    drop(adapter);
    drop(bridge);
    assert_eq!(
        requests,
        vec![
            json!({ "action": "list", "scope": "scoped" }),
            json!({ "action": "list", "scope": "all" }),
            json!({ "action": "list", "scope": "all", "refresh": true }),
        ]
    );
}

#[test]
fn bridge_errors_are_reported_without_decoding_them_as_rows() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let socket = directory.path().join("picker.sock");
    let bridge = MockBridge::start(&socket, vec!["error\tPi is unavailable\n"]);
    let mut adapter = PiModelAdapter::new(client(&socket), None);

    let error = adapter.load(false).expect_err("bridge error");
    assert!(error.to_string().contains("Pi is unavailable"));
    drop(adapter);
    drop(bridge);
}
