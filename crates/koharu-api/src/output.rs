use std::{
    io::{Cursor, Write as _},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use image::{
    ExtendedColorType, ImageEncoder as _,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use koharu_psd::PsdExportOptions;
use koharu_rasterizer::{Raster, RasterOptions, Rasterizer};
use koharu_renderer::{Frame, Renderer};
use koharu_scene::{EntityId, Snapshot};
use serde::Deserialize;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{handlers::StatusError, project::Project};

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExportFormat {
    Png,
    Psd,
}

pub(crate) async fn export_page(
    project: Project,
    page_id: EntityId,
    format: ExportFormat,
    rasterizer: Arc<Rasterizer>,
    renderer: Renderer,
) -> Result<(String, String, Vec<u8>)> {
    let snapshot = project.snapshot();

    let Some((index, page_ref)) = snapshot
        .pages()
        .enumerate()
        .find(|(_, page)| page.id().eq(&page_id))
    else {
        return Err(StatusError::NotFound("requested page not found".to_string()).into());
    };

    let page = page_ref.page()?;
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

    let (extension, mimetype) = match format {
        ExportFormat::Png => ("png", "image/png"),
        ExportFormat::Psd => ("psd", "image/vnd.adobe.photoshop"),
    };

    let bytes = render_page(page_id, format, snapshot, rasterizer, renderer).await?;

    Ok((format!("{stem}.{extension}"), mimetype.to_string(), bytes))
}

pub(crate) async fn export_pages(
    project: Project,
    pages: Vec<EntityId>,
    format: ExportFormat,
    rasterizer: Arc<Rasterizer>,
    renderer: Renderer,
) -> Result<(String, String, Vec<u8>)> {
    let snapshot = project.snapshot();
    let pages = if pages.is_empty() {
        snapshot.pages().map(|page| page.id()).collect()
    } else {
        pages
    };
    if pages.is_empty() {
        return Err(StatusError::BadRequest("there are no pages to export".to_string()).into());
    }
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

    let extension = match format {
        ExportFormat::Png => "png",
        ExportFormat::Psd => "psd",
    };

    let entries = stream::iter(jobs)
        .map(|(page_id, stem)| {
            let renderer = renderer.clone();
            let rasterizer = Arc::clone(&rasterizer);
            let snapshot = snapshot.clone();
            let format = format;
            async move {
                let bytes = render_page(page_id, format, snapshot, rasterizer, renderer).await?;
                Ok::<_, anyhow::Error>((stem, bytes))
            }
        })
        .buffer_unordered(4)
        .try_collect::<Vec<_>>()
        .await?;

    let name = format!("{}.zip", sanitize_filename(&project.name));

    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
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

    Ok((name, "application/zip".to_string(), bytes))
}

async fn render_page(
    page_id: EntityId,
    format: ExportFormat,
    snapshot: Snapshot,
    rasterizer: Arc<Rasterizer>,
    renderer: Renderer,
) -> Result<Vec<u8>> {
    let frame = renderer.render(&snapshot, page_id).await?;
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
    Ok(bytes)
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
