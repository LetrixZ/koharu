//! Pipeline: runs an ordered set of engines across one or more pages and
//! wraps each engine's output in one `Op::Batch` before applying via the
//! session's history.
//!
//! **Engines don't mutate the scene.** They return `Vec<Op>`; this driver
//! applies them transactionally (per-engine) against the active session.

pub mod artifacts;
pub mod engine;
mod engines;

pub use artifacts::Artifact;
pub use engine::{
    BoxFuture, Engine, EngineCtx, EngineInfo, EngineLoadFn, PipelineRunOptions, Registry,
    build_order,
};
pub use engines::support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use koharu_core::{Op, PageId, PipelineStep};
use koharu_runtime::RuntimeManager;
use tracing::Instrument;

use crate::pipeline::engines::llm_translate;

/// Observer for pipeline progress. `step_id` is the engine id of the step
/// about to run (or just finished); step_index / page_index are 0-based.
pub type ProgressSink = Arc<dyn Fn(ProgressTick) + Send + Sync>;

/// Observer for non-fatal step failures. Called once per failed step; the
/// pipeline skips the rest of that page's steps and moves on to the next
/// page.
pub type WarningSink = Arc<dyn Fn(WarningTick) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ProgressTick {
    /// Coarse UI-facing step tag derived from the engine's primary
    /// produced artifact. `None` for the final 100% tick where no engine
    /// is running.
    pub step: Option<PipelineStep>,
    /// Engine id (e.g. `"paddle-ocr-vl-1.6"`) for diagnostics + logs.
    pub step_id: String,
    pub step_index: usize,
    pub total_steps: usize,
    pub page_index: usize,
    pub total_pages: usize,
    pub overall_percent: u8,
}

#[derive(Debug, Clone)]
pub struct WarningTick {
    pub step_id: String,
    pub page_index: usize,
    pub total_pages: usize,
    pub message: String,
}

/// Returned by [`run`]. `warning_count == 0` means the run finished cleanly.
#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    pub warning_count: usize,
}

/// Map an engine's produced artifact to its UI step category. Stays
/// co-located with the engine metadata so adding a new engine can't
/// silently bypass the toolbar spinner — only the registered artifact
/// matters, not the engine's string id.
fn step_for(info: &EngineInfo) -> Option<PipelineStep> {
    info.produces.iter().find_map(|a| match a {
        Artifact::TextBoxes
        | Artifact::SegmentMask
        | Artifact::FontPredictions
        | Artifact::BubbleMask => Some(PipelineStep::Detect),
        Artifact::OcrText => Some(PipelineStep::Ocr),
        Artifact::Translations => Some(PipelineStep::LlmGenerate),
        Artifact::Inpainted => Some(PipelineStep::Inpaint),
        Artifact::FinalRender => Some(PipelineStep::Render),
        // Non-UI-facing artifacts (inputs, intermediate sprites) — no
        // toolbar step tag.
        _ => None,
    })
}

use crate::llm;
use crate::renderer;
use crate::session::ProjectSession;

// ---------------------------------------------------------------------------
// Spec + scope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PipelineSpec {
    pub scope: Scope,
    pub steps: Vec<String>,
    pub options: PipelineRunOptions,
}

#[derive(Debug, Clone)]
pub enum Scope {
    WholeProject,
    Pages(Vec<PageId>),
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Execute `spec` against `session`. Each engine step becomes one `Op::Batch`
/// applied via the session's history (one undo step per step per page).
///
/// A failed step on a given page is non-fatal: the rest of that page's steps
/// are skipped (they typically depend on the failed step's output), one
/// [`WarningTick`] is emitted via `warnings`, and the driver moves on to the
/// next page. The function returns the total number of per-step warnings
/// that fired, letting callers flag the run as `CompletedWithErrors`.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "info", skip_all)]
pub async fn run(
    session: Arc<ProjectSession>,
    registry: Arc<Registry>,
    runtime: Arc<RuntimeManager>,
    cpu: bool,
    llm: Arc<llm::Model>,
    renderer: Arc<renderer::Renderer>,
    spec: PipelineSpec,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressSink>,
    warnings: Option<WarningSink>,
) -> Result<RunOutcome> {
    let infos: Vec<&EngineInfo> = spec
        .steps
        .iter()
        .map(|id| Registry::find(id))
        .collect::<Result<_>>()?;
    let order = build_order(&infos)?;

    let pages = match &spec.scope {
        Scope::WholeProject => session
            .scene
            .read()
            .pages
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        Scope::Pages(ids) => ids.clone(),
    };

    let total_pages = pages.len().max(1);
    let total_steps = order.len().max(1);

    let is_paged = spec.options.paged.unwrap_or(false);
    let translator_indices: Vec<usize> = order
        .iter()
        .enumerate()
        .filter(|&(_, &i)| infos[i].produces.contains(&Artifact::Translations))
        .map(|(seq, _)| seq)
        .collect();
    let has_paged_translator = is_paged && !translator_indices.is_empty();

    // ── Paged mode ──────────────────────────────────────────────────
    //
    // When `paged=true` and there is a translator step, we restructure
    // execution so that all box-detection + OCR steps run on every page
    // before the translator runs once across all pages. This ensures all
    // pages have their detected text ready when the LLM call is made.
    //
    //   Phase 1: non-translator steps run on all pages (step-first)
    //   Phase 2: translator step runs once across all pages
    //   Phase 3: remaining steps run on all pages (step-first)
    //
    // In normal (non-paged) mode the loop is page-first so each page
    // completes all steps before the next page starts.

    if has_paged_translator {
        let result = run_paged(
            session, registry, &runtime, cpu, llm, renderer,
            &spec, &infos, &order, &pages, translator_indices,
            cancel, progress, warnings,
        )
        .await?;
        return Ok(result);
    }

    // ── Normal mode (page-first) ────────────────────────────────────

    let total_units = (total_pages * total_steps) as u64;
    let mut completed: u64 = 0;
    let mut warning_count: usize = 0;

    'pages: for (page_index, page_id) in pages.iter().enumerate() {
        for (seq, &i) in order.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let info = infos[i];

            if let Some(sink) = progress.as_ref() {
                let percent = ((completed * 100) / total_units).min(100) as u8;
                sink(ProgressTick {
                    step: step_for(info),
                    step_id: info.id.to_string(),
                    step_index: seq,
                    total_steps,
                    page_index,
                    total_pages,
                    overall_percent: percent,
                });
                tokio::task::yield_now().await;
            }

            // The page must still exist (user may have deleted it mid-run).
            if !session.scene.read().pages.contains_key(page_id) {
                completed += (total_steps - seq) as u64;
                continue 'pages;
            }

            if let Err(wc) = run_single_step(
                session.as_ref(),
                registry.as_ref(),
                &runtime,
                cpu,
                &llm,
                &renderer,
                &spec,
                info,
                *page_id,
                seq,
                page_index,
                total_pages,
                total_steps,
                &cancel,
                &mut completed,
                &mut warning_count,
                progress.as_ref(),
                warnings.as_ref(),
            )
            .await
            {
                warning_count += wc;
                continue 'pages;
            }
        }
    }

    emit_progress_done(progress.as_ref(), total_steps, total_pages);
    Ok(RunOutcome { warning_count })
}

#[allow(clippy::too_many_arguments)]
fn report_step_failure(
    engine_id: &str,
    page_id: &PageId,
    step_index: usize,
    page_index: usize,
    total_pages: usize,
    total_steps: usize,
    err: &anyhow::Error,
    warning_count: &mut usize,
    sink: Option<&WarningSink>,
) {
    let _ = total_steps;
    tracing::warn!(
        engine = engine_id,
        page = %page_id,
        step_index,
        "pipeline step failed: {err:#}"
    );
    *warning_count += 1;
    if let Some(sink) = sink {
        sink(WarningTick {
            step_id: engine_id.to_string(),
            page_index,
            total_pages,
            message: format!("{err:#}"),
        });
    }
}

/// Run one engine step on one page. Returns the extra warning count on
/// failure (0 on success) so the caller can accumulate it.
#[allow(clippy::too_many_arguments)]
async fn run_single_step(
    session: &ProjectSession,
    registry: &Registry,
    runtime: &RuntimeManager,
    cpu: bool,
    llm: &llm::Model,
    renderer: &renderer::Renderer,
    spec: &PipelineSpec,
    info: &EngineInfo,
    page_id: PageId,
    seq: usize,
    page_index: usize,
    total_pages: usize,
    total_steps: usize,
    cancel: &AtomicBool,
    completed: &mut u64,
    warning_count: &mut usize,
    progress: Option<&ProgressSink>,
    warnings: Option<&WarningSink>,
) -> Result<(), usize> {
    if cancel.load(Ordering::Relaxed) {
        return Err(0);
    }

    if let Some(sink) = progress {
        let total_units = (total_pages * total_steps) as u64;
        let percent = ((*completed * 100) / total_units.max(1)).min(100) as u8;
        sink(ProgressTick {
            step: step_for(info),
            step_id: info.id.to_string(),
            step_index: seq,
            total_steps,
            page_index,
            total_pages,
            overall_percent: percent,
        });
        tokio::task::yield_now().await;
    }

    let engine = match registry.get(info.id, runtime, cpu).await {
        Ok(e) => e,
        Err(err) => {
            report_step_failure(
                info.id,
                &page_id,
                seq,
                page_index,
                total_pages,
                total_steps,
                &err,
                warning_count,
                warnings,
            );
            *completed += (total_steps - seq) as u64;
            return Err(1);
        }
    };

    let scene_snap = session.scene_snapshot();
    let ctx = EngineCtx {
        scene: &scene_snap,
        page: page_id,
        blobs: &session.blobs,
        runtime,
        cancel,
        options: &spec.options,
        llm,
        renderer,
    };
    let step_result = async { engine.run(ctx).await }
        .instrument(tracing::info_span!("step", engine = info.id, page = %page_id))
        .await;
    let ops = match step_result {
        Ok(ops) => ops,
        Err(err) => {
            report_step_failure(
                info.id,
                &page_id,
                seq,
                page_index,
                total_pages,
                total_steps,
                &err,
                warning_count,
                warnings,
            );
            *completed += (total_steps - seq) as u64;
            return Err(1);
        }
    };
    *completed += 1;
    if ops.is_empty() {
        return Ok(());
    }
    let batch = Op::Batch {
        ops,
        label: format!("{}: page {}", info.id, page_id),
    };
    if let Err(err) = session.apply(batch) {
        report_step_failure(
            info.id,
            &page_id,
            seq,
            page_index,
            total_pages,
            total_steps,
            &err,
            warning_count,
            warnings,
        );
        return Err(1);
    }
    Ok(())
}

/// Run the translator step once across all pages (paged mode).
/// Collects text from every page, sends a single LLM request, and applies
/// translation ops for all pages.
#[allow(clippy::too_many_arguments)]
async fn run_paged_translate(
    session: &ProjectSession,
    spec: &PipelineSpec,
    llm: &llm::Model,
    info: &EngineInfo,
    pages: &[PageId],
    seq: usize,
    _page_index: usize,
    total_pages: usize,
    total_steps: usize,
    cancel: &AtomicBool,
    completed: &mut u64,
    warning_count: &mut usize,
    progress: Option<&ProgressSink>,
    warnings: Option<&WarningSink>,
) -> Result<(), usize> {
    if cancel.load(Ordering::Relaxed) {
        return Err(0);
    }

    if let Some(sink) = progress {
        let total_units = ((total_steps - 1) * total_pages + 1) as u64; // translator counts as 1
        let percent = ((*completed * 100) / total_units.max(1)).min(100) as u8;
        sink(ProgressTick {
            step: step_for(info),
            step_id: info.id.to_string(),
            step_index: seq,
            total_steps,
            page_index: 0,
            total_pages,
            overall_percent: percent,
        });
        tokio::task::yield_now().await;
    }

    let scene_snap = session.scene_snapshot();
    let first_page = pages.first().copied();
    let ops = match llm_translate::run_paged(
        &scene_snap,
        pages,
        &spec.options,
        llm,
    )
    .await
    {
        Ok(ops) => ops,
        Err(err) => {
            if let Some(pid) = first_page {
                report_step_failure(
                    info.id,
                    &pid,
                    seq,
                    0,
                    total_pages,
                    total_steps,
                    &err,
                    warning_count,
                    warnings,
                );
            }
            *completed += 1; // count the translator step
            return Err(1);
        }
    };
    *completed += 1;

    if ops.is_empty() {
        return Ok(());
    }

    let batch = Op::Batch {
        ops,
        label: format!("{}: all pages (paged)", info.id),
    };
    if let Err(err) = session.apply(batch) {
        if let Some(pid) = first_page {
            report_step_failure(
                info.id,
                &pid,
                seq,
                0,
                total_pages,
                total_steps,
                &err,
                warning_count,
                warnings,
            );
        }
        return Err(1);
    }
    Ok(())
}

/// Run the full pipeline in paged mode. Non-translator steps run on all
/// pages (step-first), the translator step runs once across all pages,
/// then remaining steps run on all pages.
#[allow(clippy::too_many_arguments)]
async fn run_paged(
    session: Arc<ProjectSession>,
    registry: Arc<Registry>,
    runtime: &RuntimeManager,
    cpu: bool,
    llm: Arc<llm::Model>,
    renderer: Arc<renderer::Renderer>,
    spec: &PipelineSpec,
    infos: &[&EngineInfo],
    order: &[usize],
    pages: &[PageId],
    translator_indices: Vec<usize>,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressSink>,
    warnings: Option<WarningSink>,
) -> Result<RunOutcome> {
    let total_pages = pages.len().max(1);
    let total_steps = order.len().max(1);
    // Count translator steps as 1 unit each (not per-page).
    let translator_count = translator_indices.len() as u64;
    let total_units = ((total_steps as u64 - translator_count) * total_pages as u64) + translator_count;
    let mut completed: u64 = 0;
    let mut warning_count: usize = 0;

    for (seq, &i) in order.iter().enumerate() {
        let info = infos[i];
        let is_translator = translator_indices.contains(&seq);

        if is_translator {
            // Phase 2: translator step — run once across all pages.
            let _ = run_paged_translate(
                &session,
                spec,
                &llm,
                info,
                pages,
                seq,
                0, // page_index is 0 for the paged step
                total_pages,
                total_steps,
                &cancel,
                &mut completed,
                &mut warning_count,
                progress.as_ref(),
                warnings.as_ref(),
            )
            .await;
            // Note: run_paged_translate returns Err(extra_warnings) on
            // failure but the pipeline should continue with remaining
            // steps. We already accumulated warning_count inside.
            continue;
        }

        // Phase 1 & 3: non-translator steps run on each page.
        for (page_index, &page_id) in pages.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }

            if let Some(sink) = progress.as_ref() {
                let percent = ((completed * 100) / total_units.max(1)).min(100) as u8;
                sink(ProgressTick {
                    step: step_for(info),
                    step_id: info.id.to_string(),
                    step_index: seq,
                    total_steps,
                    page_index,
                    total_pages,
                    overall_percent: percent,
                });
                tokio::task::yield_now().await;
            }

            // Page may have been deleted mid-run.
            if !session.scene.read().pages.contains_key(&page_id) {
                completed += total_steps as u64 - seq as u64 - 1;
                // ^ approximate: remaining non-translator steps for this page
                continue;
            }

            let engine = match registry.get(info.id, runtime, cpu).await {
                Ok(e) => e,
                Err(err) => {
                    report_step_failure(
                        info.id,
                        &page_id,
                        seq,
                        page_index,
                        total_pages,
                        total_steps,
                        &err,
                        &mut warning_count,
                        warnings.as_ref(),
                    );
                    // Skip remaining steps for this page.
                    completed += total_steps as u64 - seq as u64 - 1;
                    continue;
                }
            };

            let scene_snap = session.scene_snapshot();
            let ctx = EngineCtx {
                scene: &scene_snap,
                page: page_id,
                blobs: &session.blobs,
                runtime,
                cancel: &cancel,
                options: &spec.options,
                llm: &llm,
                renderer: &renderer,
            };
            let step_result = async { engine.run(ctx).await }
                .instrument(tracing::info_span!("step", engine = info.id, page = %page_id))
                .await;
            let ops = match step_result {
                Ok(ops) => ops,
                Err(err) => {
                    report_step_failure(
                        info.id,
                        &page_id,
                        seq,
                        page_index,
                        total_pages,
                        total_steps,
                        &err,
                        &mut warning_count,
                        warnings.as_ref(),
                    );
                    completed += total_steps as u64 - seq as u64 - 1;
                    continue;
                }
            };
            completed += 1;
            if ops.is_empty() {
                continue;
            }
            let batch = Op::Batch {
                ops,
                label: format!("{}: page {}", info.id, page_id),
            };
            if let Err(err) = session.apply(batch) {
                report_step_failure(
                    info.id,
                    &page_id,
                    seq,
                    page_index,
                    total_pages,
                    total_steps,
                    &err,
                    &mut warning_count,
                    warnings.as_ref(),
                );
                continue;
            }
        }
    }

    emit_progress_done(progress.as_ref(), total_steps, total_pages);
    Ok(RunOutcome { warning_count })
}

fn emit_progress_done(sink: Option<&ProgressSink>, total_steps: usize, total_pages: usize) {
    if let Some(sink) = sink {
        sink(ProgressTick {
            step: None,
            step_id: String::new(),
            step_index: total_steps.saturating_sub(1),
            total_steps,
            page_index: total_pages.saturating_sub(1),
            total_pages,
            overall_percent: 100,
        });
    }
}

// ---------------------------------------------------------------------------
// Engine catalog building (API surface)
// ---------------------------------------------------------------------------

use koharu_core::{EngineCatalog, EngineCatalogEntry};

/// Build the engine catalog DTO for the API.
pub fn catalog() -> EngineCatalog {
    let entry = |info: &&EngineInfo| EngineCatalogEntry {
        id: info.id.to_string(),
        name: info.name.to_string(),
        produces: info.produces.iter().map(|a| format!("{a:?}")).collect(),
    };
    EngineCatalog {
        detectors: Registry::providers(Artifact::TextBoxes)
            .iter()
            .map(entry)
            .collect(),
        font_detectors: Registry::providers(Artifact::FontPredictions)
            .iter()
            .map(entry)
            .collect(),
        segmenters: Registry::providers(Artifact::SegmentMask)
            .iter()
            .map(entry)
            .collect(),
        bubble_segmenters: Registry::providers(Artifact::BubbleMask)
            .iter()
            .map(entry)
            .collect(),
        ocr: Registry::providers(Artifact::OcrText)
            .iter()
            .map(entry)
            .collect(),
        translators: Registry::providers(Artifact::Translations)
            .iter()
            .map(entry)
            .collect(),
        inpainters: Registry::providers(Artifact::Inpainted)
            .iter()
            .map(entry)
            .collect(),
        renderers: Registry::providers(Artifact::FinalRender)
            .iter()
            .map(entry)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_includes_anime_text_detector() {
        let catalog = catalog();

        assert!(catalog.detectors.iter().any(|engine| {
            engine.id == "anime-text"
                && engine.name == "Anime Text YOLO (N)"
                && engine.produces.iter().map(String::as_str).eq(["TextBoxes"])
        }));
    }
}
