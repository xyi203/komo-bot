//! Note-vault search as a plugin (`[wiki]`). Degrades: a search backend that
//! will not open costs `wiki_search`, never the boot.

use std::sync::Arc;

use async_trait::async_trait;

use komo_tools::wiki_index::WikiIndexTool;
use komo_tools::wiki_read::WikiReadTool;
use komo_tools::wiki_search::WikiSearchTool;

use super::{Plugin, Scope, ToolCx, ToolRegistry};
use crate::services::operator_control::actions::WikiOps;

pub struct WikiPlugin;

#[async_trait]
impl Plugin for WikiPlugin {
    fn name(&self) -> &'static str {
        "wiki"
    }

    async fn setup_tools(&self, reg: &mut ToolRegistry, cx: &ToolCx<'_>) -> anyhow::Result<()> {
        let Some(wiki) = &cx.config.runtime.wiki else {
            return Ok(());
        };
        // Registered before the handles are built, and kept even if they fail:
        // a search backend that will not open costs search, not the ability to
        // read a note whose path the user or a memory already names.
        reg.tool(
            Scope::AGENTIC,
            Arc::new(WikiReadTool::new(wiki.vault.clone())),
        );

        let (index, embedder) = match wiki_handles(wiki, cx).await {
            Ok(handles) => handles,
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "wiki_search unavailable");
                return Ok(());
            }
        };
        tracing::info!(vault = %wiki.vault.display(), "wiki_search ready");
        // One runner shared by every indexing caller: this process's
        // `wiki_index` tool, `komo wiki index` over the operator channel, and
        // any cron job. Two concurrent runs over one store is not merely
        // wasteful — a rebuild resets it.
        let runner = Arc::new(komo_services::wiki_indexing::WikiIndexRunner::new(
            index.clone(),
            embedder.clone(),
            wiki.vault.clone(),
            wiki.embedding.model.clone(),
        ));
        reg.wiki_ops = Some(WikiOps {
            runner: runner.clone(),
        });
        reg.tool(
            Scope::AGENTIC,
            Arc::new(WikiSearchTool::new(index, embedder)),
        );
        reg.tool(Scope::AGENTIC, Arc::new(WikiIndexTool::new(runner)));
        Ok(())
    }
}

/// Build the note-vault handles: the index over `komo.db` and the embedding
/// client. The index is a table in a database this process already has open,
/// so there is nothing left to be unreachable — the only failure is an
/// embedding url that is not a url.
async fn wiki_handles(
    wiki: &komo_config::WikiConfig,
    cx: &ToolCx<'_>,
) -> anyhow::Result<(
    Arc<komo_infra::chunk_index::TursoChunkIndex>,
    Arc<dyn komo_core::domain::embedding::EmbeddingClient>,
)> {
    let index = cx.db.chunk_index(komo_infra::chunk_index::WIKI).await?;
    let embedder = komo_infra::embedding::OllamaEmbedder::new(
        wiki.embedding.url.clone(),
        wiki.embedding.model.clone(),
    )?;
    Ok((Arc::new(index), Arc::new(embedder)))
}
