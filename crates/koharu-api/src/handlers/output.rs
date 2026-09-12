use std::{
    io::{Cursor, Write as _},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use axum::{
    body::Bytes,
    extract::{self, Path, Query, State},
    http::header,
    response::{IntoResponse as _, Response},
};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use image::{
    ExtendedColorType, ImageEncoder as _,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use koharu_psd::PsdExportOptions;
use koharu_rasterizer::{Raster, RasterOptions, Rasterizer};
use koharu_renderer::{Frame, Renderer};
use koharu_scene::EntityId;
use serde::Deserialize;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::handlers::StatusError;

use super::{ApiResult, AppState, projects::ProjectLibrary};

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Png,
    Psd,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExportPageQuery {
    format: Option<ExportFormat>,
}

pub(crate) async fn export_page(
    State(state): State<AppState>,
    State(library): State<ProjectLibrary>,
    State(renderer): State<Renderer>,
    Query(query): Query<ExportPageQuery>,
    Path((name, page_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let project = library.open(&name).await?;
    let snapshot = project.snapshot();

    let Some((index, page_ref)) = snapshot
        .pages()
        .enumerate()
        .find(|(_, page)| page.id().to_string() == page_id)
    else {
        return Err(StatusError::NotFound("requested page not found".to_string()).into());
    };

    let page = page_ref.page()?;
    let rasterizer = state.rasterizer().await?;
    let name = page
        .label
        .trim()
        .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
    let name = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let name = sanitize_filename(name);
    let stem = format!(
        "{:04}_{}",
        index + 1,
        if name.is_empty() { "page" } else { &name }
    );

    let format = query.format.unwrap_or(ExportFormat::Png);

    let (extension, mimetype) = match format {
        ExportFormat::Png => ("png", "image/png"),
        ExportFormat::Psd => ("psd", "image/vnd.adobe.photoshop"),
    };

    let frame = renderer.render(&snapshot, page_ref.id()).await?;
    let bytes = match format {
        ExportFormat::Png => {
            let image = rasterize(Arc::clone(&rasterizer), &frame, RasterOptions::default())
                .await?
                .image;
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                let mut buffer = Vec::new();
                PngEncoder::new_with_quality(
                    &mut buffer,
                    CompressionType::Best,
                    FilterType::Adaptive,
                )
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    ExtendedColorType::Rgba8,
                )?;
                Ok(buffer)
            })
            .await
            .context("PNG export worker stopped unexpectedly")??
        }
        ExportFormat::Psd => {
            koharu_psd::export_page(
                Arc::clone(&rasterizer),
                &snapshot,
                &frame,
                &PsdExportOptions::default(),
            )
            .await?
        }
    };
    tracing::info!(
        target: "koharu_metrics",
        metric = "page_exported",
        format = ?format,
    );

    let mut response = Bytes::from(bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(mimetype),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("attachment; filename=\"{stem}.{extension}\""))
            .context("invalid archive filename for Content-Disposition header")?,
    );
    Ok(response)
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExportRequest {
    #[serde(default)]
    pages: Vec<EntityId>,
    format: ExportFormat,
}

pub(crate) async fn export_pages(
    State(state): State<AppState>,
    State(library): State<ProjectLibrary>,
    State(renderer): State<Renderer>,
    Path(name): Path<String>,
    extract::Json(payload): extract::Json<ExportRequest>,
) -> ApiResult<Response> {
    let project = library.open(&name).await?;
    let snapshot = project.snapshot();
    let pages = if payload.pages.is_empty() {
        snapshot.pages().map(|page| page.id()).collect()
    } else {
        payload.pages
    };
    if pages.is_empty() {
        return Err(StatusError::BadRequest("there are no pages to export".to_string()).into());
    }
    let rasterizer = state.rasterizer().await?;
    let jobs = pages
        .into_iter()
        .enumerate()
        .map(|(index, page_id)| {
            let page = snapshot.page(page_id)?.page()?;
            let name = page
                .label
                .trim()
                .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
            let name = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
            let name = sanitize_filename(name);
            let stem = format!(
                "{:04}_{}",
                index + 1,
                if name.is_empty() { "page" } else { &name }
            );
            Ok::<_, anyhow::Error>((page_id, stem))
        })
        .collect::<Result<Vec<_>>>()?;

    let extension = match payload.format {
        ExportFormat::Png => "png",
        ExportFormat::Psd => "psd",
    };

    let entries = stream::iter(jobs)
        .map(|(page_id, stem)| {
            let renderer = renderer.clone();
            let rasterizer = Arc::clone(&rasterizer);
            let snapshot = snapshot.clone();
            let format = payload.format;
            async move {
                let frame = renderer.render(&snapshot, page_id).await?;
                let bytes = match format {
                    ExportFormat::Png => {
                        let image =
                            rasterize(Arc::clone(&rasterizer), &frame, RasterOptions::default())
                                .await?
                                .image;
                        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                            let mut buffer = Vec::new();
                            PngEncoder::new_with_quality(
                                &mut buffer,
                                CompressionType::Best,
                                FilterType::Adaptive,
                            )
                            .write_image(
                                image.as_raw(),
                                image.width(),
                                image.height(),
                                ExtendedColorType::Rgba8,
                            )?;
                            Ok(buffer)
                        })
                        .await
                        .context("PNG export worker stopped unexpectedly")??
                    }
                    ExportFormat::Psd => {
                        koharu_psd::export_page(
                            Arc::clone(&rasterizer),
                            &snapshot,
                            &frame,
                            &PsdExportOptions::default(),
                        )
                        .await?
                    }
                };
                tracing::info!(
                    target: "koharu_metrics",
                    metric = "page_exported",
                    format = ?format,
                );
                Ok::<_, anyhow::Error>((stem, bytes))
            }
        })
        .buffer_unordered(4)
        .try_collect::<Vec<_>>()
        .await?;

    let archive_name = format!("{}.zip", sanitize_filename(&project.name));

    let zip_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (stem, bytes) in entries {
            writer.start_file(format!("{stem}.{extension}"), options)?;
            writer.write_all(&bytes)?;
        }
        Ok(writer.finish()?.into_inner())
    })
    .await
    .context("zip export worker stopped unexpectedly")??;

    let mut response = Bytes::from(zip_bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("attachment; filename=\"{archive_name}\""))
            .context("invalid archive filename for Content-Disposition header")?,
    );
    Ok(response)
}

fn sanitize_filename(input: &str) -> String {
    input
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) || character.is_control()
            {
                '_'
            } else {
                character
            }
        })
        .collect()
}

async fn rasterize(
    rasterizer: Arc<Rasterizer>,
    frame: &Frame,
    options: RasterOptions,
) -> Result<Raster> {
    let frame = frame.raster_frame()?;
    tokio::task::spawn_blocking(move || rasterizer.rasterize(&frame, options))
        .await
        .context("rasterizer worker stopped unexpectedly")?
        .map_err(Into::into)
}
