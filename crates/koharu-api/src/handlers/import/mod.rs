use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};
use image::{ImageFormat, ImageReader};
use rayon::iter::{IntoParallelIterator as _, ParallelIterator as _};
use strum::{EnumIter, EnumMessage, EnumString};

mod pdf;
mod rar;
mod zip;

#[derive(Clone, Copy, EnumIter, EnumMessage, EnumString)]
#[strum(ascii_case_insensitive)]
pub(super) enum Format {
    #[strum(
        serialize = "png",
        serialize = "jpg",
        serialize = "jpeg",
        serialize = "webp"
    )]
    Raster,
    #[strum(serialize = "cbz", serialize = "zip")]
    Zip,
    #[strum(serialize = "rar")]
    Rar,
    #[strum(serialize = "pdf")]
    Pdf,
}

#[derive(Debug)]
pub(super) struct EncodedPage {
    pub(super) name: String,
    pub(super) bytes: Vec<u8>,
}

pub(super) struct Page {
    pub(super) name: String,
    pub(super) bytes: Arc<[u8]>,
    pub(super) format: ImageFormat,
    pub(super) width: u32,
    pub(super) height: u32,
}

fn decode(path: &Path, source: EncodedPage) -> Result<Page> {
    let EncodedPage { name, bytes } = source;
    let format = image::guess_format(&bytes).with_context(|| {
        format!(
            "failed to identify imported image {} ({name})",
            path.display()
        )
    })?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes.as_slice()), format)
        .into_dimensions()
        .with_context(|| {
            format!(
                "failed to read dimensions of imported image {} ({name})",
                path.display()
            )
        })?;
    Ok(Page {
        name,
        bytes: Arc::<[u8]>::from(bytes),
        format,
        width,
        height,
    })
}

pub(super) fn import(mut images: Vec<(PathBuf, Vec<u8>)>) -> Result<Vec<Page>> {
    alphanumeric_sort::sort_slice_by_os_str_key(&mut images, |image| &image.0);
    let mut groups = images
        .into_par_iter()
        .map(|(path, data)| -> Result<Vec<Page>> {
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| extension.parse::<Format>().ok());
            let filename = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "page".to_owned());
            let encoded = match extension {
                Some(Format::Raster) => vec![EncodedPage {
                    name: filename,
                    bytes: data,
                }],
                Some(Format::Zip) => zip::extract(data, &filename)?,
                Some(Format::Rar) => rar::extract(data, &filename)?,
                Some(Format::Pdf) => pdf::render(data, &filename)?,
                None => bail!("unsupported page import path {}", path.display()),
            };
            encoded
                .into_iter()
                .map(|source| decode(&path, source))
                .collect()
        })
        .collect::<Result<Vec<_>>>()?;
    let page_count = groups.iter().map(Vec::len).sum();
    let mut pages = Vec::with_capacity(page_count);
    for group in &mut groups {
        pages.append(group);
    }
    Ok(pages)
}
