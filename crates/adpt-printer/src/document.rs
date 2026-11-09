//! Rendering common image files to PDF, then printing PDF or PostScript
//! without a driver: the printer renders the document itself, wrapped in the
//! PJL header that sets duplex and color.

use crate::{lpd, pjl, StudentId, QUEUE};
use image::{imageops::FilterType, DynamicImage, ImageDecoder, ImageReader, Rgb, RgbImage};
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, RawImage, XObjectTransform};
use serde::Deserialize;
use std::fmt;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::net::SocketAddr;
use std::path::Path;

/// What the print window offers. Each copy is a separate upload of the
/// document, so the printer never has to interpret a copy count.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrintOptions {
    pub copies: u32,
    #[serde(flatten)]
    pub settings: pjl::Settings,
}

/// The most copies one print sends; more is a typo, not a print run.
pub const MAX_COPIES: u32 = 99;

#[derive(Debug)]
pub enum PrintError {
    /// The file could not be read; nothing was sent.
    File(io::Error),
    /// Not a supported image, PDF, or PostScript document.
    NotPrintable,
    /// `copies` is outside 1..=`MAX_COPIES`.
    Copies,
    /// The next copy did not reach the printer; the `accepted` copies
    /// before it did.
    Lpd { accepted: u32, error: lpd::LpdError },
}

impl fmt::Display for PrintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(error) => write!(f, "cannot read the file: {error}"),
            Self::NotPrintable => f.write_str("not a supported image, PDF, or PostScript file"),
            Self::Copies => write!(f, "copies must be between 1 and {MAX_COPIES}"),
            Self::Lpd { accepted, error } => {
                write!(f, "after {accepted} accepted copies: {error}")
            }
        }
    }
}

impl std::error::Error for PrintError {}

/// Sends `copies` uploads of the file at `path` to the campus queue on
/// `server`, billed to `student`. Returns the uploads sent. The file is
/// read once, before anything is sent, so every copy is the same document
/// whatever happens to the file meanwhile, and the only failure after the
/// first upload is the printer's, with the copies it already accepted.
pub fn print(
    server: SocketAddr,
    student: &StudentId,
    path: &Path,
    options: &PrintOptions,
) -> Result<u32, PrintError> {
    if !(1..=MAX_COPIES).contains(&options.copies) {
        return Err(PrintError::Copies);
    }
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut file = std::fs::File::open(path).map_err(PrintError::File)?;
    let mut head = [0; 5];
    let count = file.read(&mut head).map_err(PrintError::File)?;
    if pjl::Language::detect(&head[..count]).is_some() {
        file.seek(SeekFrom::Start(0)).map_err(PrintError::File)?;
        let mut document = Vec::new();
        file.read_to_end(&mut document).map_err(PrintError::File)?;
        return print_bytes(server, student, &document, &title, options);
    }
    let pdf = image_pdf(path, &title)?;
    print_bytes(server, student, &pdf, &title, options)
}

const IMAGE_MAX_WIDTH: u32 = 12_000;
const IMAGE_MAX_HEIGHT: u32 = 12_000;
const IMAGE_MAX_PIXELS: u64 = 40_000_000;
const IMAGE_MAX_ALLOCATION: u64 = 256 * 1024 * 1024;
const PDF_DPI: f32 = 300.0;
const PAGE_MARGIN_MM: f32 = 10.0;

/// Decode one image, apply its EXIF orientation, fit it on an A4 PDF page,
/// and return PDF bytes directly. No temporary file is created.
fn image_pdf(path: &Path, title: &str) -> Result<Vec<u8>, PrintError> {
    let mut reader = ImageReader::open(path)
        .map_err(PrintError::File)?
        .with_guessed_format()
        .map_err(PrintError::File)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(IMAGE_MAX_WIDTH);
    limits.max_image_height = Some(IMAGE_MAX_HEIGHT);
    limits.max_alloc = Some(IMAGE_MAX_ALLOCATION);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| PrintError::NotPrintable)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > IMAGE_MAX_PIXELS {
        return Err(PrintError::NotPrintable);
    }
    let orientation = decoder
        .orientation()
        .map_err(|_| PrintError::NotPrintable)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| PrintError::NotPrintable)?;
    image.apply_orientation(orientation);

    // Paper has no transparent ink. Composite transparency on white so a
    // transparent PNG does not print as black on devices that ignore alpha.
    let rgba = image.to_rgba8();
    let rgb = RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let pixel = rgba.get_pixel(x, y);
        let alpha = u16::from(pixel[3]);
        let white = 255 * (255 - alpha);
        Rgb([
            ((u16::from(pixel[0]) * alpha + white + 127) / 255) as u8,
            ((u16::from(pixel[1]) * alpha + white + 127) / 255) as u8,
            ((u16::from(pixel[2]) * alpha + white + 127) / 255) as u8,
        ])
    });
    let image = DynamicImage::ImageRgb8(rgb);

    let landscape = image.width() > image.height();
    let (page_width_mm, page_height_mm) = if landscape {
        (297.0, 210.0)
    } else {
        (210.0, 297.0)
    };
    let printable_width_px =
        ((page_width_mm - 2.0 * PAGE_MARGIN_MM) * PDF_DPI / 25.4).floor() as u32;
    let printable_height_px =
        ((page_height_mm - 2.0 * PAGE_MARGIN_MM) * PDF_DPI / 25.4).floor() as u32;
    let image = image.resize(
        printable_width_px,
        printable_height_px,
        FilterType::Lanczos3,
    );
    let raw = RawImage::from_dynamic_image(image).map_err(|_| PrintError::NotPrintable)?;
    let image_width_pt = raw.width as f32 * 72.0 / PDF_DPI;
    let image_height_pt = raw.height as f32 * 72.0 / PDF_DPI;
    let page_width_pt = page_width_mm * 72.0 / 25.4;
    let page_height_pt = page_height_mm * 72.0 / 25.4;

    let mut document = PdfDocument::new(title);
    let image_id = document.add_image(&raw);
    let page = PdfPage::new(
        Mm(page_width_mm),
        Mm(page_height_mm),
        vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                translate_x: Some(Pt((page_width_pt - image_width_pt) / 2.0)),
                translate_y: Some(Pt((page_height_pt - image_height_pt) / 2.0)),
                dpi: Some(PDF_DPI),
                ..Default::default()
            },
        }],
    );
    let mut warnings = Vec::new();
    let bytes = document
        .with_pages(vec![page])
        .save(&PdfSaveOptions::default(), &mut warnings);
    if !bytes.starts_with(b"%PDF-") {
        return Err(PrintError::NotPrintable);
    }
    Ok(bytes)
}

/// Sends an already rendered document without creating an intermediate file.
pub fn print_bytes(
    server: SocketAddr,
    student: &StudentId,
    document: &[u8],
    title: &str,
    options: &PrintOptions,
) -> Result<u32, PrintError> {
    if !(1..=MAX_COPIES).contains(&options.copies) {
        return Err(PrintError::Copies);
    }
    let language = pjl::Language::detect(document).ok_or(PrintError::NotPrintable)?;
    let header = pjl::header(language, &options.settings);
    let title = lpd::sanitize(title);
    let host = lpd::local_hostname();
    let data_len = (header.len() + document.len() + pjl::JOB_END.len()) as u64;
    let mut number = lpd::first_job_number();
    for accepted in 0..options.copies {
        let job = lpd::Job {
            number,
            host: &host,
            user: student,
            title: &title,
        };
        number = (number + 1) % 1000;
        let mut data = header.as_slice().chain(document).chain(pjl::JOB_END);
        lpd::send(server, QUEUE, &job, &mut data, data_len)
            .map_err(|error| PrintError::Lpd { accepted, error })?;
    }
    Ok(options.copies)
}

#[cfg(test)]
#[path = "tests/document.rs"]
mod tests;
