pub const THUMB_SIZE: [u32; 2] = [120, 90];
/// Larger cache used by the Photos-style gallery grid.
pub const GALLERY_THUMB_SIZE: [u32; 2] = [480, 360];
/// Small thumbnail used for on-demand AI analysis. This is deliberately much
/// cheaper than generating a 480x360 image for every photo on a huge drive.
pub const ANALYSIS_THUMB_SIZE: [u32; 2] = [224, 168];
pub const THUMB_CAPTION_HEIGHT: u32 = 24;
pub const MAX_THREADS: usize = 4;

use std::{
    collections::{HashMap, HashSet},
    fs::{create_dir_all, File},
    hash::{DefaultHasher, Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail, Context, Result};
use image::{imageops, DynamicImage, GenericImageView};
use log::{debug, error, trace, warn};

use crate::image_loader::open_image;

#[derive(Debug, Default, Clone)]
pub struct Thumbnails {
    /// The known thumbnail ids. This is used to avoid re-generating known thumbnails
    ids: HashSet<PathBuf>,
    /// Memoised source -> cache path mapping. Without this, every visible tile
    /// on a USB drive would open the source file and hash it again on every
    /// frame just to find out whether its thumbnail exists.
    known: HashMap<(PathBuf, String), PathBuf>,
    /// Number of thumbnails being created at a given time
    pool: Arc<Mutex<usize>>,
}

struct JobSlot(Arc<Mutex<usize>>);

impl Drop for JobSlot {
    fn drop(&mut self) {
        let mut jobs = self.0.lock().unwrap();
        *jobs = jobs.saturating_sub(1);
    }
}

impl Thumbnails {
    pub fn get<P: AsRef<Path>>(&mut self, path: P) -> Result<PathBuf> {
        self.get_sized(path, THUMB_SIZE, "")
    }

    /// Request the large gallery thumbnail. It is generated on a background
    /// thread and returned on a later frame.
    pub fn get_gallery<P: AsRef<Path>>(&mut self, path: P) -> Result<PathBuf> {
        self.get_gallery_for_display(path, true)
    }

    /// Gallery thumbnail with an embedded-preview fast path.
    ///
    /// When `allow_full` is false (during scrolling) the cheap camera preview
    /// is used without starting a full-size decode. Once scrolling stops the
    /// caller sets it to true and the high-quality thumbnail is generated.
    pub fn get_gallery_for_display<P: AsRef<Path>>(
        &mut self,
        path: P,
        allow_full: bool,
    ) -> Result<PathBuf> {
        let source = path.as_ref();
        let gallery = self.known_path(source, "_gallery");
        if gallery.exists() {
            return Ok(gallery);
        }
        let preview = self.known_path(source, "_preview");
        if preview.exists() {
            if allow_full {
                let _ = self.get_sized(source, GALLERY_THUMB_SIZE, "_gallery");
            }
            return Ok(preview);
        }

        // Start a cheap embedded-EXIF preview job. If the JPEG has no embedded
        // preview, the job falls back to generating the full gallery image.
        self.start_preview_job(source, GALLERY_THUMB_SIZE)?;
        bail!("Thumbnail not yet present.");
    }

    /// Small thumbnail for the AI classifier. Reuses an existing gallery or
    /// preview, otherwise generates a cheap 224x168 copy.
    pub fn get_analysis<P: AsRef<Path>>(&mut self, path: P) -> Result<PathBuf> {
        if let Some(existing) = self.existing_analysis(path.as_ref()) {
            return Ok(existing);
        }
        self.get_sized(path, ANALYSIS_THUMB_SIZE, "_ml")
    }

    /// True if the source already has any thumbnail the classifier can use,
    /// without requesting any new work.
    pub fn existing_analysis<P: AsRef<Path>>(&mut self, path: P) -> Option<PathBuf> {
        if let Some(existing) = self.existing_gallery(path.as_ref()) {
            return Some(existing);
        }
        let preview = self.known_path(path.as_ref(), "_preview");
        if preview.exists() {
            return Some(preview);
        }
        let cached = self.known_path(path.as_ref(), "_ml");
        cached.exists().then_some(cached)
    }

    pub fn existing_gallery<P: AsRef<Path>>(&mut self, path: P) -> Option<PathBuf> {
        let cached = self.known_path(path.as_ref(), "_gallery");
        cached.exists().then_some(cached)
    }

    fn start_preview_job(&mut self, source: &Path, target: [u32; 2]) -> Result<()> {
        if !get_disk_cache_path()?.exists() {
            create_dir_all(get_disk_cache_path()?)?;
        }
        let preview_path = self.known_path(source, "_preview");
        if self.ids.contains(&preview_path) {
            bail!("Preview is still processing or failed in the past.");
        }
        let slot = {
            let mut jobs = self.pool.lock().unwrap();
            if *jobs >= MAX_THREADS {
                bail!("Thumbnail pool is busy.");
            }
            *jobs += 1;
            JobSlot(self.pool.clone())
        };
        let source = source.to_path_buf();
        let job_preview_path = preview_path.clone();
        let spawned = std::thread::Builder::new()
            .name("photos-preview".into())
            .spawn(move || {
                let _slot = slot;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        if let Some(preview) = embedded_jpeg_preview(&source) {
                            if from_existing_sized(&job_preview_path, &preview, target).is_ok() {
                                debug!(
                                    "Embedded EXIF preview {}x{} used for {}",
                                    preview.width(),
                                    preview.height(),
                                    source.display()
                                );
                                return Ok(());
                            }
                        }
                        // No embedded preview (or it could not be used): go
                        // straight to the full gallery thumbnail.
                        generate_sized(&source, target, "_gallery")
                    }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error!("Error generating thumbnail preview: {e}"),
                    Err(_) => error!("Thumbnail preview decoder panicked"),
                }
            });
        if let Err(e) = spawned {
            bail!("Could not start preview worker: {e}");
        }
        self.ids.insert(preview_path);
        bail!("Thumbnail not yet present.");
    }

    fn known_path(&mut self, path: &Path, suffix: &str) -> PathBuf {
        let key = (path.to_path_buf(), suffix.to_string());
        if let Some(known) = self.known.get(&key) {
            return known.clone();
        }
        let cached = get_cached_path_suffix(path, suffix);
        self.known.insert(key, cached.clone());
        cached
    }

    fn get_sized<P: AsRef<Path>>(
        &mut self,
        path: P,
        size: [u32; 2],
        suffix: &str,
    ) -> Result<PathBuf> {
        trace!("Thumbnail requested for {}", path.as_ref().display());

        if !get_disk_cache_path()?.exists() {
            warn!("Thumbnail cache dir missing, creating it");
            create_dir_all(&get_disk_cache_path()?)?;
        }
        let cached_path = self.known_path(path.as_ref(), suffix);

        // The thumbnail is missing and needs to be generated
        if !cached_path.exists() {
            if self.ids.contains(&cached_path) {
                bail!("Thumbnail is still processing or failed in the past.");
            }
            debug!("\tThumbnail missing");
            // Reserve a worker slot *before* spawning, so a grid with hundreds
            // of visible placeholders cannot create hundreds of waiting
            // threads. Callers simply retry on a later frame.
            let slot = {
                let mut jobs = self.pool.lock().unwrap();
                if *jobs >= MAX_THREADS {
                    bail!("Thumbnail pool is busy.");
                }
                *jobs += 1;
                JobSlot(self.pool.clone())
            };
            let fp = path.as_ref().to_path_buf();
            let suffix = suffix.to_string();
            let spawned = std::thread::Builder::new()
                .name("photos-thumb".into())
                .spawn(move || {
                    // The guard releases the slot even if a decoder panics.
                    let _slot = slot;
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        generate_sized(&fp, size, &suffix)
                    })) {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => error!("Error generating thumbnail: {e}"),
                        Err(_) => error!("Thumbnail decoder panicked"),
                    }
                });
            if let Err(e) = spawned {
                bail!("Could not start thumbnail worker: {e}");
            }
            self.ids.insert(cached_path);
            bail!("Thumbnail not yet present.");
        }
        Ok(cached_path)
    }

    /// How many thumbnail jobs are currently running or queued.
    pub fn active_jobs(&self) -> usize {
        *self.pool.lock().unwrap()
    }

    /// Forget cached path lookups, e.g. after a folder refresh.
    pub fn clear_known(&mut self) {
        self.known.clear();
    }
}

pub fn path_to_id<P: AsRef<Path>>(path: P) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    path.as_ref().hash(&mut hasher);
    let size = File::open(path)
        .and_then(|f| f.metadata().map(|m| m.len()))
        .unwrap_or_default();
    PathBuf::from(format!("{}_{size}", hasher.finish())).with_extension("png")
}

pub fn get_disk_cache_path() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .ok_or(anyhow!("Can't get cache dir"))?
        .join(crate::APP_ID)
        .join("thumbnails"))
}

pub fn get_cached_path<P: AsRef<Path>>(path: P) -> PathBuf {
    get_cached_path_suffix(path, "")
}

pub fn get_cached_path_suffix<P: AsRef<Path>>(path: P, suffix: &str) -> PathBuf {
    let id = path_to_id(path);
    let stem = id
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // The large gallery cache uses JPEG (far smaller, and it is only ever a
    // screen-resolution preview). The classic 120x90 cache stays PNG.
    let extension = if suffix.is_empty() { "png" } else { "jpg" };
    get_disk_cache_path()
        .unwrap_or_default()
        .join(format!("{stem}{suffix}"))
        .with_extension(extension)
}

pub fn generate<P: AsRef<Path>>(source_path: P) -> Result<()> {
    generate_sized(source_path, THUMB_SIZE, "")
}

pub fn generate_sized<P: AsRef<Path>>(source_path: P, size: [u32; 2], suffix: &str) -> Result<()> {
    let dest_path = get_cached_path_suffix(&source_path, suffix);
    debug!(
        "\tGen thumbnail for {} to {}",
        source_path.as_ref().display(),
        dest_path.display()
    );

    // Small thumbnails can often come straight from a camera's embedded EXIF
    // preview. Reading ~128 KB is dramatically cheaper than decoding the full
    // photo on a slow USB drive.
    if suffix != "_gallery" {
        if let Some(preview) = embedded_jpeg_preview(source_path.as_ref()) {
            from_existing_sized(dest_path, &preview, size)?;
            return Ok(());
        }
    }

    // JPEGs are by far the most common format on photo drives. Decode them
    // directly at a reduced DCT scale instead of decoding 20+ megapixels and
    // then throwing most of them away. This is 4-48x less pixel work for
    // 480x360 / 224x168 thumbnails.
    #[cfg(feature = "turbo")]
    if let Some(fast) = jpeg_scaled_thumbnail(source_path.as_ref(), size) {
        from_existing_sized(dest_path, &fast, size)?;
        return Ok(());
    }

    let f = open_image(source_path.as_ref(), None, None)?;
    let i = f.recv()?.get_image().context("Can't get buffer")?;

    debug!("\tOpened {}", source_path.as_ref().display());

    from_existing_sized(dest_path, &i, size)?;
    Ok(())
}

/// Extract the small JPEG preview that most cameras and phones embed in the
/// EXIF APP1 segment. Only the first 128 KB of the file is read.
fn embedded_jpeg_preview(source: &Path) -> Option<DynamicImage> {
    let ext = source.extension()?.to_string_lossy().to_ascii_lowercase();
    if ext != "jpg" && ext != "jpeg" {
        return None;
    }
    let mut data = Vec::with_capacity(128 * 1024);
    File::open(source)
        .ok()?
        .take(128 * 1024)
        .read_to_end(&mut data)
        .ok()?;
    let tiff = jpeg_exif_tiff(&data)?;
    let exif = exif::Reader::new().read_raw(tiff.to_vec()).ok()?;
    let offset = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let length = exif
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let thumbnail = exif.buf().get(offset..offset.checked_add(length)?)?;
    image::load_from_memory(thumbnail).ok()
}

/// Return the TIFF payload of the first APP1 EXIF segment of a JPEG.
fn jpeg_exif_tiff(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let mut marker = data[i + 1];
        while marker == 0xFF {
            i += 1;
            if i + 1 >= data.len() {
                return None;
            }
            marker = data[i + 1];
        }
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            0xDA => return None,
            _ => {}
        }
        let len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
        if len < 2 || i + 2 + len > data.len() {
            return None;
        }
        if marker == 0xE1 {
            let payload = &data[i + 4..i + 2 + len];
            if payload.starts_with(b"Exif\0\0") {
                return Some(&payload[6..]);
            }
        }
        i += 2 + len;
    }
    None
}

#[cfg(feature = "turbo")]
fn jpeg_scaled_thumbnail(source: &Path, target: [u32; 2]) -> Option<DynamicImage> {
    let ext = source.extension()?.to_string_lossy().to_ascii_lowercase();
    if ext != "jpg" && ext != "jpeg" {
        return None;
    }
    let data = std::fs::read(source).ok()?;
    let mut decoder = turbojpeg::Decompressor::new().ok()?;
    let header = decoder.read_header(&data).ok()?;

    // Pick the smallest supported DCT scale that still provides enough pixels
    // for the requested crop/resize.
    let mut best: Option<(usize, turbojpeg::ScalingFactor)> = None;
    for factor in turbojpeg::Decompressor::supported_scaling_factors() {
        let w = factor.scale(header.width);
        let h = factor.scale(header.height);
        if w >= target[0] as usize && h >= target[1] as usize {
            let pixels = w.saturating_mul(h);
            if best.map_or(true, |(best_pixels, _)| pixels < best_pixels) {
                best = Some((pixels, factor));
            }
        }
    }
    let factor = best
        .map(|(_, f)| f)
        .unwrap_or(turbojpeg::ScalingFactor::ONE);
    decoder.set_scaling_factor(factor).ok()?;
    let scaled = header.scaled(factor);

    let mut image = turbojpeg::Image {
        pixels: vec![0u8; 4 * scaled.width * scaled.height],
        width: scaled.width,
        pitch: 4 * scaled.width,
        height: scaled.height,
        format: turbojpeg::PixelFormat::RGBA,
    };
    decoder.decompress(&data, image.as_deref_mut()).ok()?;
    let rgba = image::RgbaImage::from_raw(scaled.width as u32, scaled.height as u32, image.pixels)?;
    Some(DynamicImage::ImageRgba8(rgba))
}

pub fn from_existing<P: AsRef<Path>>(dest_path: P, image: &DynamicImage) -> Result<()> {
    from_existing_sized(dest_path, image, THUMB_SIZE)
}

pub fn from_existing_sized<P: AsRef<Path>>(
    dest_path: P,
    image: &DynamicImage,
    target: [u32; 2],
) -> Result<()> {
    let dest_path = dest_path.as_ref().to_path_buf();
    debug!("TMB=> Original image size: {:?}", image.dimensions());

    let target_width = target[0];
    let target_height = target[1];
    let desired_aspect = target_width as f32 / target_height as f32;

    let (orig_width, orig_height) = image.dimensions();
    let orig_aspect = orig_width as f32 / orig_height as f32;

    let (crop_width, crop_height) = if orig_aspect > desired_aspect {
        // Image is too wide. Crop horizontally.
        let crop_w = (desired_aspect * orig_height as f32).round() as u32;
        (crop_w, orig_height)
    } else {
        // Image is too tall or just right width. Crop vertically.
        let crop_h = (orig_width as f32 / desired_aspect).round() as u32;
        (orig_width, crop_h)
    };

    let x_offset = (orig_width - crop_width) / 2;
    let y_offset = (orig_height - crop_height) / 2;

    let cropped_img =
        imageops::crop_imm(image, x_offset, y_offset, crop_width, crop_height).to_image();

    let mut d = DynamicImage::ImageRgba8(cropped_img);
    let op = crate::image_editing::ImageOperation::Resize {
        dimensions: (target_width, target_height),
        aspect: false,
        filter: crate::image_editing::ScaleFilter::Bilinear,
    };
    op.process_image(&mut d)?;

    // Write atomically: readers (egui's image loader and the classifier) must
    // never see a half-written thumbnail.
    let ext = dest_path
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "png".into());
    let stem = dest_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "thumb".into());
    let tmp = dest_path.with_file_name(format!(".{stem}.tmp.{ext}"));
    d.save(&tmp)?;
    if dest_path.exists() {
        let _ = std::fs::remove_file(&dest_path);
    }
    if let Err(e) = std::fs::rename(&tmp, &dest_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    Ok(())
}

#[test]
fn test_thumbs() {
    std::env::set_var("RUST_LOG", "debug");
    let _ = env_logger::try_init();
    let mut thumbs = Thumbnails::default();
    _ = thumbs.get("tests/rust.png");
    _ = thumbs.get("tests/ultrahigh.png");
    _ = thumbs.get("tests/mohsen-karimi.webp");
    std::thread::sleep(std::time::Duration::from_millis(1000));
}
