//! Offline photo categorisation.
//!
//! A tiny SqueezeNet 1.1 (ImageNet-1000) model is embedded in the binary and
//! executed with tract, a pure-Rust ONNX runtime. Inference runs on a
//! background thread so browsing and zooming stay at full frame rate.
//!
//! Results are cached on disk keyed by path + mtime, which makes the second
//! launch instant.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use image::imageops::FilterType;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};

const MODEL: &[u8] = include_bytes!("../res/models/squeezenet1.1-7.onnx");
const SYNSET: &str = include_str!("../res/models/synset.txt");
const MODEL_TAG: &str = "squeezenet1.1-imagenet-v1";
const CACHE_VERSION: u32 = 5;

/// Broad categories the gallery can filter by.
pub const CATEGORIES: &[CategoryDef] = &[
    CategoryDef {
        id: "trees",
        name: "Trees & plants",
        short: "Trees",
    },
    CategoryDef {
        id: "cats",
        name: "Cats",
        short: "Cats",
    },
    CategoryDef {
        id: "dogs",
        name: "Dogs",
        short: "Dogs",
    },
    CategoryDef {
        id: "birds",
        name: "Birds",
        short: "Birds",
    },
    CategoryDef {
        id: "insects",
        name: "Insects & bugs",
        short: "Insects",
    },
    CategoryDef {
        id: "animals",
        name: "Animals",
        short: "Animals",
    },
    CategoryDef {
        id: "food",
        name: "Food & drink",
        short: "Food",
    },
    CategoryDef {
        id: "vehicles",
        name: "Vehicles",
        short: "Vehicles",
    },
    CategoryDef {
        id: "places",
        name: "Places & nature",
        short: "Places",
    },
    CategoryDef {
        id: "objects",
        name: "Objects",
        short: "Objects",
    },
];

#[derive(Debug, Clone, Copy)]
pub struct CategoryDef {
    pub id: &'static str,
    pub name: &'static str,
    pub short: &'static str,
}

pub fn category_name(id: &str) -> &'static str {
    CATEGORIES
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.name)
        .unwrap_or("Other")
}

pub fn category_static(id: &str) -> &'static str {
    CATEGORIES
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.id)
        .unwrap_or("objects")
}

/// A cached classification for one photo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlLabel {
    /// Short ImageNet label, e.g. "tabby, tabby cat" (first synonym).
    pub label: String,
    /// Broad category id, see [`CATEGORIES`].
    pub category: String,
    pub confidence: f32,
    /// Best few labels with probabilities.
    pub top: Vec<(String, f32)>,
    /// Source file modified time (milliseconds since the epoch).
    pub modified_ms: u64,
    /// Model identifier, so replacing the model invalidates old results.
    #[serde(default)]
    pub model: String,
    /// True when a descriptive filename overrode an uncertain model result.
    #[serde(default)]
    pub hinted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    model: String,
    entries: HashMap<PathBuf, MlLabel>,
}

#[derive(Serialize)]
struct CacheFileRef<'a> {
    version: u32,
    model: &'a str,
    entries: &'a HashMap<PathBuf, MlLabel>,
}

struct Request {
    source: PathBuf,
    thumbnail: PathBuf,
    modified_ms: u64,
}

enum Event {
    Ready,
    Done { source: PathBuf, label: MlLabel },
    Failed { source: PathBuf, message: String },
}

/// Handle to the background classifier and its persistent cache.
pub struct MlService {
    tx: Option<Sender<Request>>,
    rx: Option<Receiver<Event>>,
    /// Labels available right now.
    pub cache: HashMap<PathBuf, MlLabel>,
    pending: HashSet<PathBuf>,
    pub enabled: bool,
    pub ready: bool,
    fatal: bool,
    async_error: Option<String>,
    /// Number of photos classified during this session.
    pub session_done: usize,
    /// Total number of photos currently queued.
    pub session_total: usize,
    cache_path: PathBuf,
    dirty: bool,
    last_save: Instant,
}

impl Default for MlService {
    fn default() -> Self {
        let data_dir = dirs::data_local_dir().unwrap_or_default();
        let cache_dir = data_dir.join(crate::APP_ID);
        let cache_path = cache_dir.join("ml_cache.json");
        let legacy_cache_path = data_dir.join("photos").join("ml_cache.json");

        if !cache_path.exists() && legacy_cache_path.exists() {
            let _ = std::fs::create_dir_all(&cache_dir);
            if let Err(e) = std::fs::rename(&legacy_cache_path, &cache_path) {
                warn!("Could not migrate the old ML cache: {e}");
            }
        }

        let cache = load_cache(&cache_path)
            .or_else(|e| {
                if legacy_cache_path.exists() {
                    load_cache(&legacy_cache_path)
                } else {
                    Err(e)
                }
            })
            .unwrap_or_else(|e| {
                debug!("No usable ML cache: {e}");
                HashMap::new()
            });
        Self {
            tx: None,
            rx: None,
            cache,
            pending: HashSet::new(),
            enabled: true,
            ready: false,
            fatal: false,
            async_error: None,
            session_done: 0,
            session_total: 0,
            cache_path,
            dirty: false,
            last_save: Instant::now(),
        }
    }
}

impl MlService {
    pub fn error(&self) -> Option<&str> {
        self.async_error.as_deref()
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// True if the label for this exact version of the file is known.
    pub fn is_analyzed(&self, path: &Path, modified_ms: u64) -> bool {
        self.cache
            .get(path)
            .map(|l| l.modified_ms == modified_ms && l.model == MODEL_TAG)
            .unwrap_or(false)
    }

    pub fn label_for(&self, path: &Path, modified_ms: u64) -> Option<&MlLabel> {
        self.cache
            .get(path)
            .filter(|l| l.modified_ms == modified_ms && l.model == MODEL_TAG)
    }

    /// Queue a photo for classification. The thumbnail must already exist.
    pub fn request(&mut self, source: &Path, thumbnail: &Path, modified_ms: u64) {
        if !self.enabled
            || self.fatal
            || self.pending.contains(source)
            || self.is_analyzed(source, modified_ms)
        {
            return;
        }
        self.ensure_worker();
        if let Some(tx) = &self.tx {
            if tx
                .send(Request {
                    source: source.to_path_buf(),
                    thumbnail: thumbnail.to_path_buf(),
                    modified_ms,
                })
                .is_ok()
            {
                self.pending.insert(source.to_path_buf());
            }
        }
    }

    /// Drain background results. Returns true when anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                Event::Ready => {
                    info!("Photo classifier ready");
                    self.ready = true;
                    changed = true;
                }
                Event::Done { source, label, .. } => {
                    self.pending.remove(&source);
                    self.cache.insert(source.clone(), label);
                    self.session_done += 1;
                    self.dirty = true;
                    changed = true;
                    debug!("Classified {}", source.display());
                }
                Event::Failed { source, message } => {
                    if source.as_os_str().is_empty() {
                        // Model-load failure: stop retrying every photo.
                        self.pending.clear();
                        self.async_error = Some(message);
                        self.fatal = true;
                    } else {
                        self.pending.remove(&source);
                        warn!("Could not classify {}: {message}", source.display());
                    }
                    changed = true;
                }
            }
        }
        self.session_total = self
            .session_total
            .max(self.session_done + self.pending.len());
        // Writing a 20k-entry cache on every few seconds stalls the UI. The
        // cache is also saved on pause/exit, so a longer interval is safe.
        if self.dirty && self.last_save.elapsed() > Duration::from_secs(45) {
            self.save();
        }
        changed
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Err(e) = save_cache(&self.cache_path, &self.cache) {
            warn!("Could not save ML cache: {e}");
        } else {
            self.dirty = false;
            self.last_save = Instant::now();
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.pending.clear();
        }
    }

    fn ensure_worker(&mut self) {
        if self.tx.is_some() || self.fatal {
            return;
        }
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        self.tx = Some(req_tx);
        self.rx = Some(ev_rx);
        std::thread::Builder::new()
            .name("photos-ml".into())
            .spawn(move || worker(req_rx, ev_tx))
            .ok();
    }
}

impl Drop for MlService {
    fn drop(&mut self) {
        self.save();
    }
}

fn load_cache(path: &Path) -> Result<HashMap<PathBuf, MlLabel>> {
    let file = File::open(path)?;
    let cache: CacheFile = serde_json::from_reader(file)?;
    if cache.version != CACHE_VERSION || cache.model != MODEL_TAG {
        debug!("Discarding stale ML cache");
        return Ok(HashMap::new());
    }
    Ok(cache.entries)
}

fn save_cache(path: &Path, entries: &HashMap<PathBuf, MlLabel>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cache = CacheFileRef {
        version: CACHE_VERSION,
        model: MODEL_TAG,
        entries,
    };
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp)?;
        serde_json::to_writer(&mut f, &cache)?;
        f.flush()?;
    }
    std::fs::rename(&tmp, path).or_else(|_| {
        // Windows can fail to replace an open file; fall back to a direct write.
        let mut f = File::create(path)?;
        serde_json::to_writer(&mut f, &cache)?;
        let _ = std::fs::remove_file(&tmp);
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

fn worker(rx: Receiver<Request>, tx: Sender<Event>) {
    let labels = synset_labels();
    if labels.len() != 1000 {
        let _ = tx.send(Event::Failed {
            source: PathBuf::new(),
            message: format!("ImageNet labels look wrong ({} found)", labels.len()),
        });
        return;
    }
    info!("Loading SqueezeNet photo classifier");
    let runnable = match build_model() {
        Ok(m) => m,
        Err(e) => {
            error!("Could not load the photo classifier: {e:#}");
            let _ = tx.send(Event::Failed {
                source: PathBuf::new(),
                message: format!("{e:#}"),
            });
            return;
        }
    };
    let _ = tx.send(Event::Ready);

    while let Ok(req) = rx.recv() {
        match classify(&runnable, &req, &labels) {
            Ok(label) => {
                let _ = tx.send(Event::Done {
                    source: req.source,
                    label,
                });
            }
            Err(e) => {
                let _ = tx.send(Event::Failed {
                    source: req.source,
                    message: format!("{e:#}"),
                });
            }
        }
    }
}

fn build_model() -> Result<Arc<tract_onnx::prelude::TypedRunnableModel>> {
    use tract_onnx::prelude::*;
    let mut reader = Cursor::new(MODEL);
    let model = tract_onnx::onnx()
        .model_for_read(&mut reader)
        .context("reading embedded ONNX model")?;
    let model = model
        .with_input_fact(0, f32::fact([1, 3, 224, 224]).into())
        .context("setting classifier input shape")?;
    let model = model.into_optimized().context("optimizing model")?;
    model.into_runnable().context("preparing model")
}

fn classify(
    runnable: &Arc<tract_onnx::prelude::TypedRunnableModel>,
    req: &Request,
    labels: &[String],
) -> Result<MlLabel> {
    use tract_onnx::prelude::*;

    let input = preprocess(&req.thumbnail)?;
    let shape = [1usize, 3, 224, 224];
    let tensor = Tensor::from_shape(&shape, &input).context("building input tensor")?;
    let outputs = runnable
        .run(tvec!(tensor.into_tvalue()))
        .context("classifier inference")?;
    let view = outputs[0]
        .to_plain_array_view::<f32>()
        .context("classifier output")?;

    let mut values: Vec<f32> = view.iter().copied().collect();
    if values.len() < 1000 {
        return Err(anyhow!(
            "classifier returned {} values, expected 1000",
            values.len()
        ));
    }
    values.truncate(1000);

    let sum: f32 = values.iter().sum();
    let non_negative = values.iter().all(|v| *v >= -1e-5);
    let probs = if non_negative && (0.9..=1.1).contains(&sum) {
        values
    } else {
        softmax(&values)
    };

    let mut ranked: Vec<usize> = (0..probs.len()).collect();
    ranked.sort_unstable_by(|a, b| {
        probs[*b]
            .partial_cmp(&probs[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let top: Vec<(String, f32)> = ranked
        .iter()
        .take(5)
        .map(|i| (short_label(&labels[*i]), probs[*i]))
        .collect();
    let best = ranked.first().copied().unwrap_or_default();
    let label = top
        .first()
        .map(|t| t.0.clone())
        .unwrap_or_else(|| "unknown".into());
    let mut category = categorize(&labels[best]).to_string();
    // Filenames are a useful prior for the many pictures that SqueezeNet
    // cannot name confidently (e.g. a landscape with a tree and a barn). Only
    // let the filename win when the model is unsure.
    let hint = heuristic_category(&req.source);
    let mut hinted = false;
    if hint != "objects" && (category == "objects" || probs[best] < 0.35) {
        category = hint.to_string();
        hinted = true;
    }

    Ok(MlLabel {
        label,
        category,
        confidence: probs[best],
        top,
        modified_ms: req.modified_ms,
        model: MODEL_TAG.into(),
        hinted,
    })
}

fn preprocess(path: &Path) -> Result<Vec<f32>> {
    let img = image::open(path)
        .with_context(|| format!("opening thumbnail {}", path.display()))?
        .to_rgb8();

    // ImageNet-style preprocessing: resize the short side to 256, then take a
    // 224x224 centre crop.
    let (w, h) = img.dimensions();
    let scale = 256.0 / w.min(h).max(1) as f32;
    let nw = (w as f32 * scale).round().max(224.0) as u32;
    let nh = (h as f32 * scale).round().max(224.0) as u32;
    let resized = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    let x = (nw - 224) / 2;
    let y = (nh - 224) / 2;
    let resized = image::imageops::crop_imm(&resized, x, y, 224, 224).to_image();

    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    let mut out = vec![0.0f32; 3 * 224 * 224];
    for (i, p) in resized.pixels().enumerate() {
        for c in 0..3 {
            out[c * 224 * 224 + i] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c];
        }
    }
    Ok(out)
}

fn softmax(values: &[f32]) -> Vec<f32> {
    let max = values.iter().copied().fold(f32::MIN, f32::max);
    let mut exps: Vec<f32> = values.iter().map(|v| (v - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum > 0.0 {
        for v in &mut exps {
            *v /= sum;
        }
    }
    exps
}

fn synset_labels() -> Vec<String> {
    SYNSET
        .lines()
        .filter_map(|line| {
            let (_, names) = line.split_once(' ')?;
            Some(names.trim().to_string())
        })
        .collect()
}

fn short_label(label: &str) -> String {
    let first = label.split(',').next().unwrap_or(label).trim();
    if first.is_empty() {
        label.trim().to_string()
    } else {
        first.to_string()
    }
}

pub fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

pub fn modified_ms_for(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .map(|m| modified_ms(&m))
        .unwrap_or_default()
}

/// Map an ImageNet label to one of the broad gallery categories.
fn has_kw(label: &str, keyword: &str) -> bool {
    if keyword.contains(|c: char| !c.is_alphanumeric()) {
        // Phrases ("hot dog", "sea lion") and hyphenated compounds.
        label.contains(keyword)
    } else {
        // Whole-word match, so "ant" does not match "elephant" or "plant".
        label
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word == keyword)
    }
}

pub fn categorize(raw: &str) -> &'static str {
    let s = raw.to_lowercase();
    // Hot dog is food, not a dog.
    if s.contains("hot dog") || s.contains("hotdog") || s.contains("red hot") {
        return "food";
    }

    const FOOD: &[&str] = &[
        "food",
        "dish",
        "pizza",
        "burrito",
        "potpie",
        "carbonara",
        "meat loaf",
        "guacamole",
        "consomme",
        "trifle",
        "ice cream",
        "ice lolly",
        "chocolate sauce",
        "dough",
        "bagel",
        "pretzel",
        "french loaf",
        "cheeseburger",
        "mashed potato",
        "head cabbage",
        "broccoli",
        "cauliflower",
        "zucchini",
        "spaghetti squash",
        "acorn squash",
        "butternut squash",
        "cucumber",
        "artichoke",
        "bell pepper",
        "cardoon",
        "mushroom",
        "agaric",
        "bolete",
        "earthstar",
        "stinkhorn",
        "hen-of-the-woods",
        "gyromitra",
        "banana",
        "orange",
        "lemon",
        "fig",
        "pineapple",
        "strawberry",
        "apple",
        "granny smith",
        "pomegranate",
        "corn",
        "espresso",
        "cup",
        "coffee mug",
        "beer bottle",
        "beer glass",
        "wine bottle",
        "cocktail shaker",
        "red wine",
        "eggnog",
    ];
    if FOOD.iter().any(|k| has_kw(&s, k)) {
        return "food";
    }

    const CATS: &[&str] = &["cat", "tabby", "lynx"];
    if CATS.iter().any(|k| has_kw(&s, k)) {
        return "cats";
    }

    const DOGS: &[&str] = &[
        "dog",
        "retriever",
        "terrier",
        "spaniel",
        "setter",
        "sheepdog",
        "collie",
        "poodle",
        "schnauzer",
        "pinscher",
        "hound",
        "husky",
        "malamute",
        "dalmatian",
        "chihuahua",
        "pug",
        "boxer",
        "bulldog",
        "mastiff",
        "chow",
        "pomeranian",
        "papillon",
        "leonberg",
        "newfoundland",
        "great dane",
        "eskimo dog",
        "mexican hairless",
        "dingo",
        "borzoi",
        "kuvasz",
        "great pyrenees",
        "basenji",
        "beagle",
        "bloodhound",
        "bluetick",
        "coonhound",
        "foxhound",
        "redbone",
        "saluki",
        "deerhound",
        "weimaraner",
        "whippet",
        "greyhound",
        "elkhound",
        "otterhound",
        "kelpie",
        "komondor",
        "schipperke",
        "groenendael",
        "malinois",
        "briard",
        "bouvier",
        "rottweiler",
        "doberman",
        "bernese",
        "appenzeller",
        "entlebucher",
        "samoyed",
        "keeshond",
        "griffon",
        "pembroke",
        "cardigan",
        "maltese",
        "pekinese",
        "shih-tzu",
        "saint bernard",
        "corgi",
        "pointer",
        "vizsla",
        "clumber",
        "springer",
        "sussex",
        "wheaten",
        "lakeland",
        "sealyham",
        "bedlington",
        "dandie",
        "norfolk",
        "norwich",
        "cairn",
        "airedale",
        "affepinscher",
        "affenpinscher",
        "brabancon",
        "bull mastiff",
        "french bulldog",
        "tibetan mastiff",
        "tibetan terrier",
    ];
    if DOGS.iter().any(|k| has_kw(&s, k)) {
        return "dogs";
    }

    const BIRDS: &[&str] = &[
        "bird",
        "hen",
        "cock",
        "ostrich",
        "brambling",
        "goldfinch",
        "finch",
        "junco",
        "bunting",
        "robin",
        "bulbul",
        "jay",
        "magpie",
        "chickadee",
        "ouzel",
        "kite",
        "eagle",
        "vulture",
        "owl",
        "peacock",
        "quail",
        "partridge",
        "parrot",
        "macaw",
        "cockatoo",
        "lorikeet",
        "coucal",
        "bee eater",
        "hornbill",
        "hummingbird",
        "jacamar",
        "toucan",
        "duck",
        "goose",
        "swan",
        "flamingo",
        "heron",
        "egret",
        "bittern",
        "crane",
        "limpkin",
        "gallinule",
        "coot",
        "bustard",
        "turnstone",
        "sandpiper",
        "redshank",
        "dowitcher",
        "oystercatcher",
        "pelican",
        "penguin",
        "albatross",
        "gull",
        "loon",
        "grebe",
        "rail",
        "crake",
        "grouse",
        "ptarmigan",
        "turkey",
        "guinea fowl",
        "drake",
        "merganser",
        "goosander",
        "shelduck",
        "mallard",
        "teal",
        "wigeon",
        "pintail",
        "scaup",
        "canvasback",
        "bufflehead",
        "goldeneye",
        "smew",
        "eider",
        "scoter",
        "warbler",
        "wren",
        "sparrow",
        "starling",
        "thrush",
        "waxwing",
        "tanager",
        "cardinal",
        "grosbeak",
        "towhee",
        "chat",
        "ovenbird",
        "catbird",
        "mockingbird",
        "shrike",
        "butcherbird",
        "drongo",
        "bird of paradise",
        "lyrebird",
        "scrub bird",
        "bowerbird",
    ];
    if BIRDS.iter().any(|k| has_kw(&s, k)) {
        return "birds";
    }

    const INSECTS: &[&str] = &[
        "insect",
        "bee",
        "ant",
        "butterfly",
        "dragonfly",
        "damselfly",
        "grasshopper",
        "beetle",
        "moth",
        "cockroach",
        "mantis",
        "cicada",
        "leafhopper",
        "lacewing",
        "walking stick",
        "fly",
        "mosquito",
        "cricket",
        "weevil",
        "ladybug",
        "lycaenid",
        "ringlet",
        "monarch",
        "admiral",
        "sulphur butterfly",
        "cabbage butterfly",
        "spider",
        "tarantula",
        "scorpion",
        "tick",
        "centipede",
        "millipede",
        "worm",
        "snail",
    ];
    if INSECTS.iter().any(|k| has_kw(&s, k)) {
        return "insects";
    }

    const ANIMALS: &[&str] = &[
        "animal",
        "mammal",
        "fox",
        "wolf",
        "coyote",
        "bear",
        "leopard",
        "jaguar",
        "lion",
        "tiger",
        "cheetah",
        "elephant",
        "zebra",
        "giraffe",
        "hippopotamus",
        "rhinoceros",
        "monkey",
        "ape",
        "gorilla",
        "chimpanzee",
        "orangutan",
        "gibbon",
        "baboon",
        "macaque",
        "lemur",
        "deer",
        "elk",
        "moose",
        "bison",
        "ox",
        "ram",
        "sheep",
        "goat",
        "antelope",
        "gazelle",
        "camel",
        "llama",
        "horse",
        "sorrel",
        "pig",
        "boar",
        "hog",
        "cow",
        "hare",
        "rabbit",
        "squirrel",
        "beaver",
        "porcupine",
        "hamster",
        "guinea pig",
        "mouse",
        "rat",
        "shrew",
        "mole",
        "hedgehog",
        "bat",
        "seal",
        "sea lion",
        "walrus",
        "whale",
        "dolphin",
        "shark",
        "ray",
        "stingray",
        "eel",
        "salmon",
        "trout",
        "goldfish",
        "carp",
        "puffer",
        "fish",
        "crab",
        "lobster",
        "crayfish",
        "shrimp",
        "jellyfish",
        "starfish",
        "sea anemone",
        "coral",
        "frog",
        "toad",
        "salamander",
        "newt",
        "lizard",
        "gecko",
        "iguana",
        "chameleon",
        "snake",
        "python",
        "boa",
        "cobra",
        "rattlesnake",
        "alligator",
        "crocodile",
        "turtle",
        "tortoise",
        "terrapin",
        "platypus",
        "echidna",
        "kangaroo",
        "koala",
        "wombat",
        "opossum",
        "armadillo",
        "sloth",
        "anteater",
        "pangolin",
        "meerkat",
        "mongoose",
        "otter",
        "badger",
        "skunk",
        "weasel",
        "mink",
        "ferret",
        "polecat",
        "marten",
        "wolverine",
        "hyena",
        "jackal",
        "panda",
        "buffalo",
        "wild boar",
        "hartebeest",
        "impala",
        "ibex",
        "bighorn",
    ];
    if ANIMALS.iter().any(|k| has_kw(&s, k)) {
        return "animals";
    }

    const TREES: &[&str] = &[
        "tree",
        "oak",
        "yew",
        "linden",
        "basswood",
        "sycamore",
        "willow",
        "poplar",
        "chestnut",
        "buckeye",
        "acorn",
        "conker",
        "pine",
        "fir",
        "spruce",
        "larch",
        "cypress",
        "redwood",
        "sequoia",
        "flower",
        "daisy",
        "rose",
        "sunflower",
        "orchid",
        "lily",
        "magnolia",
        "poppy",
        "cornflower",
        "iris",
        "pansy",
        "lady's slipper",
        "coral fungus",
        "forest",
        "rapeseed",
    ];
    if TREES.iter().any(|k| has_kw(&s, k)) {
        return "trees";
    }

    const VEHICLES: &[&str] = &[
        "car",
        "truck",
        "bus",
        "train",
        "boat",
        "ship",
        "bicycle",
        "motorcycle",
        "moped",
        "scooter",
        "airplane",
        "airliner",
        "warplane",
        "spacecraft",
        "submarine",
        "tank",
        "taxi",
        "limousine",
        "convertible",
        "beach wagon",
        "minivan",
        "pickup",
        "trailer",
        "moving van",
        "garbage truck",
        "fire engine",
        "ambulance",
        "police van",
        "snowmobile",
        "canoe",
        "gondola",
        "lifeboat",
        "speedboat",
        "container ship",
        "fireboat",
        "paddlewheel",
        "yawl",
        "catamaran",
        "trimaran",
        "schooner",
        "kayak",
        "aircraft",
        "helicopter",
        "locomotive",
        "steam locomotive",
        "electric locomotive",
        "bullet train",
        "freight car",
        "passenger car",
        "streetcar",
        "trolleybus",
        "school bus",
        "minibus",
        "jeep",
        "sport utility vehicle",
        "sports car",
        "coupe",
        "cab",
        "hansom",
    ];
    if VEHICLES.iter().any(|k| has_kw(&s, k)) {
        return "vehicles";
    }

    const PLACES: &[&str] = &[
        "castle",
        "palace",
        "church",
        "mosque",
        "monastery",
        "library",
        "barn",
        "boathouse",
        "lighthouse",
        "beacon",
        "pier",
        "dock",
        "fountain",
        "cliff",
        "valley",
        "lakeside",
        "seashore",
        "promontory",
        "geyser",
        "volcano",
        "coral reef",
        "sandbar",
        "patio",
        "pergola",
        "maze",
        "bridge",
        "viaduct",
        "dam",
        "suspension bridge",
        "steel arch bridge",
        "railway bridge",
        "planetarium",
        "observatory",
        "ruin",
        "arch",
        "dome",
        "victory",
        "monument",
        "mausoleum",
        "stupa",
        "altar",
        "bannister",
        "belfry",
        "bell tower",
        "cowbarn",
        "birdhouse",
        "water tower",
        "stone wall",
        "picket fence",
        "chainlink fence",
        "fence",
        "greenhouse",
        "megalith",
        "dolmen",
    ];
    if PLACES.iter().any(|k| has_kw(&s, k)) {
        return "places";
    }

    "objects"
}

#[allow(dead_code)]
fn heuristic_category(path: &Path) -> &'static str {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    const HINTS: &[(&str, &str)] = &[
        ("cat", "cats"),
        ("kitty", "cats"),
        ("dog", "dogs"),
        ("puppy", "dogs"),
        ("tree", "trees"),
        ("forest", "trees"),
        ("flower", "trees"),
        ("plant", "trees"),
        ("garden", "trees"),
        ("bird", "birds"),
        ("insect", "insects"),
        ("bug", "insects"),
        ("bee", "insects"),
        ("butterfly", "insects"),
        ("food", "food"),
        ("pizza", "food"),
        ("cake", "food"),
        ("meal", "food"),
        ("car", "vehicles"),
        ("bike", "vehicles"),
        ("beach", "places"),
        ("mountain", "places"),
        ("sunset", "places"),
    ];
    for (key, category) in HINTS {
        if has_kw(&name, key) {
            return category;
        }
    }
    "objects"
}

#[allow(dead_code)]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_cover_the_friendly_buckets() {
        assert_eq!(categorize("tabby, tabby cat"), "cats");
        assert_eq!(categorize("golden retriever"), "dogs");
        assert_eq!(
            categorize("hot dog, hotdog, red hot"),
            "food",
            "hot dogs are food, not dogs"
        );
        assert_eq!(categorize("honey bee, Apis mellifera"), "insects");
        assert_eq!(
            categorize("African elephant, Loxodonta africana"),
            "animals",
            "the word 'ant' inside elephant must not make it an insect"
        );
        assert_eq!(categorize("pizza, pizza pie"), "food");
        assert_eq!(categorize("oak, oak tree"), "trees");
        assert_eq!(categorize("sports car, sport car"), "vehicles");
        assert_eq!(categorize("castle"), "places");
    }

    #[test]
    #[ignore = "runs the full embedded ONNX model"]
    fn embedded_model_produces_predictions() {
        let labels = synset_labels();
        assert_eq!(labels.len(), 1000);
        let runnable = build_model().expect("model should load");
        let req = Request {
            source: PathBuf::from("tests/rust.png"),
            thumbnail: PathBuf::from("tests/rust.png"),
            modified_ms: 0,
        };
        let label = classify(&runnable, &req, &labels).expect("inference should work");
        assert_eq!(label.top.len(), 5);
        assert!(!label.label.is_empty());
        assert!(!label.category.is_empty());
        println!("{label:?}");
    }
}
