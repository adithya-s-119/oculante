pub const THUMB_SIZE: [u32; 2] = [120, 90];
/// Larger cache used by the Photos-style gallery grid.
pub const GALLERY_THUMB_SIZE: [u32; 2] = [480, 360];
pub const THUMB_CAPTION_HEIGHT: u32 = 24;
pub const MAX_THREADS: usize = 6;

use std::{
    collections::HashSet,
    fs::{create_dir_all, File},
    hash::{DefaultHasher, Hash, Hasher},
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
    /// Number of thumbnails being created at a given time
    pool: Arc<Mutex<usize>>,
}

impl Thumbnails {
    pub fn get<P: AsRef<Path>>(&mut self, path: P) -> Result<PathBuf> {
        self.get_sized(path, THUMB_SIZE, "")
    }

    /// Request the large gallery thumbnail. It is generated on a background
    /// thread and returned on a later frame.
    pub fn get_gallery<P: AsRef<Path>>(&mut self, path: P) -> Result<PathBuf> {
        self.get_sized(path, GALLERY_THUMB_SIZE, "_gallery")
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
        let cached_path = get_cached_path_suffix(&path, suffix);

        // The thumbnail is missing and needs to be generated
        if !cached_path.exists() {
            if self.ids.contains(&cached_path) {
                bail!("Thumbnail is still processing or failed in the past.");
            }
            debug!("\tThumbnail missing");
            // Reserve a worker slot *before* spawning, so a grid with hundreds
            // of visible placeholders cannot create hundreds of waiting
            // threads. Callers simply retry on a later frame.
            {
                let mut jobs = self.pool.lock().unwrap();
                if *jobs >= MAX_THREADS {
                    bail!("Thumbnail pool is busy.");
                }
                *jobs += 1;
            }
            let fp = path.as_ref().to_path_buf();
            let pool = self.pool.clone();
            let suffix = suffix.to_string();
            std::thread::spawn(move || {
                if let Err(e) = generate_sized(&fp, size, &suffix) {
                    error!("Error generating thumbnail: {e}");
                }
                let mut jobs = pool.lock().unwrap();
                *jobs = jobs.saturating_sub(1);
            });
            self.ids.insert(cached_path);
            bail!("Thumbnail not yet present.");
        }
        Ok(cached_path)
    }

    /// How many thumbnail jobs are currently running or queued.
    pub fn active_jobs(&self) -> usize {
        *self.pool.lock().unwrap()
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
        .join("oculante")
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
    let f = open_image(source_path.as_ref(), None, None)?;
    let i = f.recv()?.get_image().context("Can't get buffer")?;

    debug!("\tOpened {}", source_path.as_ref().display());

    from_existing_sized(dest_path, &i, size)?;
    Ok(())
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
