mod git;
mod heartbeats;

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use aw_client_rust::AwClient;
use chrono::{DateTime, TimeDelta, Utc};
use clap::{value_parser, Arg, Command};
use heartbeats::{Data, Heartbeats, Sender};
use percent_encoding::percent_decode_str;
use serde_json::Value;
use tokio::{sync::Mutex, time::MissedTickBehavior};
use tower_lsp::{jsonrpc, lsp_types::*, Client, LanguageServer, LspService, Server};

// Zed tells a language server nothing about focus changes, tab switches or scrolling,
// only about edits, and about cursor moves through code action requests. So Zed is
// considered in use for this long after the last of them, in place of being focused.
const IDLE: TimeDelta = TimeDelta::seconds(60);
const INTERVAL: Duration = Duration::from_secs(60);
const PULSETIME: f64 = 80.0;

fn decode_uri_path(uri: &Url) -> String {
    percent_decode_str(uri.path())
        .decode_utf8_lossy()
        .to_string()
}

#[derive(Debug)]
struct File {
    uri: String,
    language: Option<String>,
}

impl File {
    fn new(uri: &Url, language: Option<String>) -> Self {
        Self {
            uri: decode_uri_path(uri),
            language,
        }
    }
}

// The last edit or cursor move
struct Activity {
    file: File,
    at: DateTime<Utc>,
}

struct AwSender {
    client: AwClient,
    bucket_id: String,
    bucket_ready: AtomicBool,
}

impl Sender for AwSender {
    async fn send(&self, data: &Data, timestamp: DateTime<Utc>) {
        if !self.bucket_ready.load(Ordering::Relaxed) {
            if let Err(e) = self
                .client
                .create_bucket_simple(&self.bucket_id, "app.editor.activity")
                .await
            {
                eprintln!("Could not create ActivityWatch bucket, received error {e:?}");
                return;
            }
            self.bucket_ready.store(true, Ordering::Relaxed);
        }

        // Duration 0 because heartbeats https://docs.activitywatch.net/en/latest/buckets-and-events.html#id7
        let event = aw_client_rust::Event::new(timestamp, TimeDelta::zero(), data.clone());
        if let Err(e) = self
            .client
            .heartbeat(&self.bucket_id, &event, PULSETIME)
            .await
        {
            // The server may be down or restarted: create the bucket again next time
            self.bucket_ready.store(false, Ordering::Relaxed);
            eprintln!("Received error trying to send a heartbeat to the server: {e:?}");
        }
    }
}

struct ActivityWatchLanguageServer {
    client: Client,
    heartbeats: Mutex<Heartbeats<AwSender>>,
    activity: Mutex<Option<Activity>>,
    file_languages: Mutex<HashMap<String, String>>,
    project: ArcSwap<Option<String>>,
}

impl ActivityWatchLanguageServer {
    async fn data(&self, file: &File) -> Data {
        let mut data = serde_json::Map::new();
        data.insert("file".to_string(), Value::String(file.uri.clone()));
        let language = match &file.language {
            Some(l) => Some(l.clone()),
            None => self.file_languages.lock().await.get(&file.uri).cloned(),
        };

        if let Some(project) = (**self.project.load()).as_ref() {
            data.insert("project".to_string(), Value::String(project.clone()));
        }

        if let Some(language) = language {
            data.insert("language".to_string(), Value::String(language));
        }

        if let Some(dir) = Path::new(&file.uri).parent() {
            let branch = git::branch(dir).await;
            if let Some(repository) = git::repository(dir, branch.as_deref()).await {
                data.insert("repository".to_string(), Value::String(repository));
            }
            if let Some(branch) = branch {
                data.insert("branch".to_string(), Value::String(branch));
            }
        }
        data
    }

    // An edit or a cursor move: starts a new event right away if the file or its
    // branch changed, or if Zed was not in use. Otherwise the next tick extends it.
    async fn activity(&self, file: File) {
        let now = Utc::now();
        let data = self.data(&file).await;
        *self.activity.lock().await = Some(Activity { file, at: now });
        self.heartbeats
            .lock()
            .await
            .beat_if_changed(data, now)
            .await;
    }

    // Every minute: extends the current event while Zed is in use, else ends it
    async fn tick(&self) {
        let now = Utc::now();
        let activity = self.activity.lock().await;
        let Some(activity) = activity.as_ref() else {
            return;
        };
        if now - activity.at <= IDLE {
            // Read again each time, so a checkout is picked up
            let data = self.data(&activity.file).await;
            self.heartbeats.lock().await.beat(data, now).await;
        } else {
            // Nothing was sent after that, as ticks only beat until then
            self.heartbeats.lock().await.close(activity.at + IDLE).await;
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for ActivityWatchLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        if let Some(folders) = params.workspace_folders {
            if let Some(folder) = folders.get(0) {
                let path = folder.uri.path().to_string();
                self.project.swap(Arc::new(Some(path)));
            }
        }
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                // Zed requests code actions on every cursor move: the only way to know
                // the editor is in use, and which file, when the user does not type
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                "ActivityWatch language server initialized",
            )
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        if let Some(activity) = self.activity.lock().await.as_ref() {
            let end = Utc::now().min(activity.at + IDLE);
            self.heartbeats.lock().await.close(end).await;
        }
        Ok(())
    }

    // Not an activity: Zed opens files in the background, such as all the
    // restored tabs at startup
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        // This is a minor memory leak and ideally we'd look for close events
        // to remove entries
        self.file_languages.lock().await.insert(
            decode_uri_path(&params.text_document.uri),
            params.text_document.language_id,
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.activity(File::new(&params.text_document.uri, None))
            .await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.activity(File::new(&params.text_document.uri, None))
            .await;
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> jsonrpc::Result<Option<CodeActionResponse>> {
        self.activity(File::new(&params.text_document.uri, None))
            .await;
        Ok(None)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let matches = Command::new("activitywatch_ls")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Sacha Korban <sk@sachk.com>")
        .about("A simple ActivityWatch language server watcher")
        .arg(
            Arg::new("host")
                .short('a')
                .long("host")
                .help("The host of the ActivityWatch server to connect to")
                .required(false)
                .default_value("localhost"),
        )
        .arg(
            Arg::new("port")
                .value_parser(value_parser!(u16))
                .short('p')
                .long("port")
                .help("The ActivityWatch server port to connect to on the host")
                .required(false)
                .default_value("5600"),
        )
        .get_matches();

    // Note that AwClient does not support https
    // TODO: this sucks and i hate the alternatives too lol
    let host: &String = matches.get_one("host").unwrap();
    let port: &u16 = matches.get_one("port").unwrap();

    const CLIENT_NAME: &str = "aw-watcher-zed";
    let aw_client = match AwClient::new(host, *port, CLIENT_NAME) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Could not connect to ActivityWatch Server, received error {e:?}");
            return;
        }
    };

    // Created with the first heartbeat, so the server can be started after Zed
    let bucket_id = format!("{CLIENT_NAME}-bucket_{}", aw_client.hostname);

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        let server = Arc::new(ActivityWatchLanguageServer {
            client,
            heartbeats: Mutex::new(Heartbeats::new(AwSender {
                client: aw_client,
                bucket_id,
                bucket_ready: AtomicBool::new(false),
            })),
            activity: Mutex::new(None),
            file_languages: Mutex::new(HashMap::new()),
            project: ArcSwap::from_pointee(None),
        });
        let ticking = server.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                ticking.tick().await;
            }
        });
        server
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
