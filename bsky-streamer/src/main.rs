// use anyhow::{anyhow, Result};
use crate::types::Frame;
use atrium_api::{
    app::bsky::feed::{post::Record, Post as BPost},
    com::atproto::sync::subscribe_repos::{Commit, NSID},
    types::{CidLink, Collection},
};
use color_eyre::{eyre::eyre, Result};
use futures::StreamExt;
use surrealdb::{engine::remote::ws::Ws, Surreal};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use types::{CidOld, CommitHandler, PostData, Subscription};
use update_rate::RateCounter;

mod db_types;
mod exporter;
mod types;

struct RepoSubscription {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl RepoSubscription {
    async fn new(bgs: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let (stream, _) = connect_async(format!("wss://{bgs}/xrpc/{NSID}")).await?;
        Ok(RepoSubscription { stream })
    }

    async fn run(
        &mut self,
        mut handler: impl CommitHandler,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut commit_count = 0;
        while let Some(result) = self.next().await {
            if let Ok(Frame::Message(Some(t), message)) = result {
                if t.as_str() == "#commit" {
                    let commit = serde_ipld_dagcbor::from_reader(message.body.as_slice())?;
                    if let Err(err) = handler.handle_commit(&commit).await {
                        tracing::error!("FAILED: {err:?}");
                    }
                    commit_count += 1;
                    if commit_count >= 20 {
                        // Update cursor logic here
                        // Assuming `update_cursor` is a method on the handler
                        handler.update_cursor(commit.seq as u64).await?;
                        commit_count = 0;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Subscription for RepoSubscription {
    async fn next(&mut self) -> Option<Result<Frame, <Frame as TryFrom<&[u8]>>::Error>> {
        if let Some(Ok(Message::Binary(data))) = self.stream.next().await {
            Some(Frame::try_from(data.as_slice()))
        } else {
            None
        }
    }
}

// #[derive(Debug)]
struct FirehoseConsumer {
    rate_counter: update_rate::DiscreteRateCounter,
    // post_count: u64,
    // start_time: std::time::Instant,
    // exporter: dyn exporter::Exporter,
    exporter: Box<dyn exporter::Exporter>,
}

impl FirehoseConsumer {
    fn new(exporter: Box<dyn exporter::Exporter>) -> Self {
        FirehoseConsumer {
            rate_counter: update_rate::DiscreteRateCounter::new(50),
            // post_count: 0,
            // start_time: std::time::Instant::now(),
            exporter,
        }
    }
}

impl CommitHandler for FirehoseConsumer {
    async fn update_cursor(&self, seq: u64) -> Result<()> {
        // no-op for now, I need to find a way to update the websocket params
        tracing::trace!("Updating cursor to {}", seq);
        // Implement the logic to update the cursor here
        Ok(())
    }
    #[tracing::instrument(skip(self, commit))]
    async fn handle_commit(&mut self, commit: &Commit) -> Result<()> {
        for op in &commit.ops {
            if !self.is_post_creation(op) {
                continue;
            }

            let record = self.extract_post_record(op, &commit.blocks).await?;
            // get repo

            let post = PostData::new(commit.repo.clone(), commit.commit.clone(), record);

            let post = db_types::Post::new(post);

            tracing::trace!(?post, "Received post");

            self.exporter.export(post).await?;

            // let jsonl = serde_json::to_string(&post)?;

            // // append to file called "posts.jsonl"

            // let mut file = tokio::fs::OpenOptions::new()
            //     .append(true)
            //     .create(true)
            //     .open("posts.jsonl")
            //     .await?;

            // tokio::io::AsyncWriteExt::write_all(&mut file, format!("{}\n", jsonl).as_bytes()).await?;

            self.update_stats();
        }
        Ok(())
    }
}

impl FirehoseConsumer {
    fn is_post_creation(
        &self,
        op: &atrium_api::com::atproto::sync::subscribe_repos::RepoOp,
    ) -> bool {
        matches!(op.action.as_str(), "create") && op.path.split('/').next() == Some(BPost::NSID)
    }

    async fn extract_post_record(
        &self,
        op: &atrium_api::com::atproto::sync::subscribe_repos::RepoOp,
        mut blocks: &[u8],
    ) -> Result<Record> {
        let (items, _) = rs_car::car_read_all(&mut blocks, true).await?;

        let (_, item) = items
            .iter()
            .find(|(cid, _)| {
                let converted_cid = CidLink(
                    CidOld::from(*cid)
                        .try_into()
                        .expect("invalid CID conversion"),
                );
                Some(converted_cid) == op.cid
            })
            .ok_or_else(|| {
                eyre!(
                    "Could not find item with operation cid {:?} out of {} items",
                    op.cid,
                    items.len()
                )
            })?;

        Ok(serde_ipld_dagcbor::from_reader(&mut item.as_slice())?)
    }
    // #[tracing::instrument(skip(self))]
    fn update_stats(&mut self) {
        self.rate_counter.update();
        if self.rate_counter.rate_age_cycles() == 0 {
            tracing::info!(
                "Ingest rate: {rate:.2} items/s",
                rate = self.rate_counter.rate()
            );
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(false)
        .with_level(true)
        .with_file(false)
        .compact()
        .with_line_number(false)
        .with_env_filter("info")
        .init();
    let db = Surreal::new::<Ws>("localhost:8000").await?;

    db.use_ns("bsky.network").use_db("bsky").await?;
    // Load schema from file
    let schema = include_str!("schema.surql");

    tracing::info!("Loading schema");
    db.query(schema).await?;

    let surreal_exporter = exporter::SurrealDbExporter::new(db);

    let consumer = FirehoseConsumer::new(Box::new(surreal_exporter));

    RepoSubscription::new("bsky.network")
        .await
        .unwrap()
        .run(consumer)
        .await
        .unwrap();

    Ok(())
}
