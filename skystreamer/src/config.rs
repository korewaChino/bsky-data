use clap::{Parser, ValueEnum};
use color_eyre::Result;
use surrealdb::{opt::auth::Root, Surreal};

use crate::{exporter, FirehoseConsumer};
// use crate::
#[derive(Debug, ValueEnum, Clone)]
pub enum SurrealAuthType {
    /// Use a token for authentication
    Token,
    /// Root-level authentication
    Root,
    /// Namespace authentication
    Namespace,
}

#[derive(Debug, ValueEnum, Clone, Default)]
pub enum ExporterType {
    /// Export to a JSONL (JSON Lines) file
    Jsonl,
    /// Export to a CSV file
    Csv,
    /// Export to a SurrealDB instance
    #[default]
    Surrealdb,
}

#[derive(Parser, Debug, Clone)]
pub struct FileExporterOptions {
    /// Path to the file to export to
    #[clap(
        short = 'f',
        long,
        required_if_eq("exporter", "jsonl"),
        required_if_eq("exporter", "csv"),
        env = "FILE_EXPORT_PATH",
        group = "file_exporter"
    )]
    pub file_path: Option<String>,
}

#[derive(Parser, Debug, Clone)]
pub struct SurrealDbConn {
    /// SurrealDB endpoint
    #[clap(
        short = 'e',
        long,
        // default_value_if("surreal_conn_type", "Websocket", "ws://localhost:8000"),
        // default_value_if("surreal_conn_type", "Http", "http://localhost:8000"),
        required_if_eq("exporter", "surrealdb"),
        default_value = "ws://localhost:8000",
        env = "SURREAL_ENDPOINT",
        group = "surrealdb"
    )]
    pub surreal_endpoint: String,

    /// Authentication type for SurrealDB
    #[clap(
        short = 'a',
        long,
        // default_value = "none",
        env = "SURREAL_AUTH_TYPE",
        group = "surrealdb"
    )]
    pub auth_type: Option<SurrealAuthType>,

    /// Token for authentication
    /// Required if `auth_type` is `Token`
    #[clap(
        short = 'k',
        long,
        required_if_eq("auth_type", "Token"),
        env = "SURREAL_TOKEN",
        group = "surrealdb"
    )]
    pub token: Option<String>,
    /// Username for authentication
    /// Required if `auth_type` is `Root` or `Namespace`
    #[clap(
        short = 'u',
        long,
        required_if_eq_any([("auth_type", "Root"), ("auth_type", "Namespace")]),
        env = "SURREAL_USERNAME",
        group = "surrealdb"
    )]
    pub username: Option<String>,
    /// Password for authentication
    /// Required if `auth_type` is `UsernamePassword`
    /// This field is marked as `sensitive` so the value will be redacted in logs
    #[clap(
        short = 'p',
        long,
        required_if_eq("auth_type", "UsernamePassword"),
        env = "SURREAL_PASSWORD",
        group = "surrealdb"
    )]
    pub password: Option<String>,

    /// Namespace to use in SurrealDB
    #[clap(
        short = 'n',
        long,
        default_value = "bsky.network",
        env = "SURREAL_NAMESPACE"
    )]
    pub namespace: String,

    /// Database to use in SurrealDB
    #[clap(short = 'd', long, default_value = "bsky", env = "SURREAL_DATABASE")]
    pub database: String,
}

impl SurrealDbConn {
    pub async fn get_surreal_conn(&self) -> Result<Surreal<surrealdb::engine::any::Any>> {
        let endpoint = self.surreal_endpoint.clone();
        let conn: Surreal<surrealdb::engine::any::Any> = Surreal::init();
        conn.connect(endpoint).await?;

        match self.auth_type {
            Some(SurrealAuthType::Token) => {
                conn.authenticate(self.token.as_deref().unwrap()).await?;
            }
            Some(SurrealAuthType::Root) => {
                conn.signin(Root {
                    username: self.username.as_deref().unwrap(),
                    password: self.password.as_deref().unwrap(),
                })
                .await?;
            }
            Some(SurrealAuthType::Namespace) => {
                conn.signin(surrealdb::opt::auth::Namespace {
                    username: self.username.as_deref().unwrap(),
                    password: self.password.as_deref().unwrap(),
                    namespace: &self.namespace.clone(),
                })
                .await?;
            }
            _ => {}
        }
        conn.use_ns(&self.namespace).use_db(&self.database).await?;

        // run schema
        let schema = include_str!("schema.surql");
        tracing::info!("Loading schema");
        conn.query(schema).await?;

        Ok(conn)
    }
}

#[derive(Parser, Debug)]
#[clap(name = "skystreamer", about = "A tool for streaming data to SurrealDB")]
pub struct Config {
    /// SurrealDB endpoint
    #[clap(flatten)]
    pub surreal_conn: SurrealDbConn,
    #[clap(short = 'E', long, default_value = "surrealdb", env = "EXPORTER")]
    pub exporter: ExporterType,
    #[clap(flatten)]
    pub file_exporter: FileExporterOptions,
}

impl Config {
    pub async fn subscribe(&self) -> Result<FirehoseConsumer> {
        let exporter = match self.exporter {
            ExporterType::Jsonl => {
                let file_path = self.file_exporter.file_path.as_ref().unwrap();
                let file = tokio::fs::File::create(file_path).await?;
                Box::new(exporter::JsonlExporter::new(file)) as Box<dyn exporter::Exporter>
            }
            ExporterType::Csv => {
                let file_path = self.file_exporter.file_path.as_ref().unwrap();
                let file = tokio::fs::File::create(file_path).await?;
                Box::new(exporter::CsvExporter::new(file)) as Box<dyn exporter::Exporter>
            }
            ExporterType::Surrealdb => {
                let conn = self.surreal_conn.get_surreal_conn().await?;
                Box::new(exporter::SurrealDbExporter::new(conn)) as Box<dyn exporter::Exporter>
            }
        };

        Ok(FirehoseConsumer::new(exporter))
    }
}
