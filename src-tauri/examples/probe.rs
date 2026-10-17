//! Retrieval diagnostics: rank stored photo vectors against a query using the
//! same EmbeddingGemma build the app ships.
//!
//! ```text
//! cargo run --release --example probe -- "两个人站着" "两个小孩" "receipt"
//! ```

use embeddinggemma_core::{
    Content, ContentEmbeddingsRequest, DeviceRequest, Embedder, EmbedderOptions, EmbeddingTask,
    EmbeddingsRequest, ModelFiles, Modalities, OneOrMany, resolve_source,
};
use rusqlite::{Connection, params};

fn model_dir() -> std::path::PathBuf {
    std::env::var("SHIGUANG_MODEL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join("Library/Application Support/com.shiguang.photo-organizer/models")
        })
}

fn database_path() -> std::path::PathBuf {
    std::env::var("SHIGUANG_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join("Library/Application Support/com.shiguang.photo-organizer/shiguang.sqlite3")
        })
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (dot, na, nb) = a.iter().zip(b).fold((0.0f32, 0.0f32, 0.0f32), |(d, na, nb), (x, y)| {
        (d + x * y, na + x * x, nb + y * y)
    });
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn normalize(v: &mut [f32]) {
    let norm = dot(v, v).sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Leading principal directions of `rows` by power iteration with deflation.
fn principal_directions(rows: &[Vec<f32>], k: usize, mean: &[f32]) -> Vec<Vec<f32>> {
    let dim = mean.len();
    let mut components: Vec<Vec<f32>> = Vec::new();
    for index in 0..k {
        let mut v = vec![0.0f32; dim];
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = if i == index % dim { 1.0 } else { 0.0 };
        }
        for _ in 0..24 {
            let mut next = vec![0.0f32; dim];
            for row in rows {
                let centered: Vec<f32> = row.iter().zip(mean).map(|(x, m)| x - m).collect();
                let scale = dot(&centered, &v);
                for (slot, value) in next.iter_mut().zip(&centered) {
                    *slot += scale * value;
                }
            }
            for component in &components {
                let projection = dot(&next, component);
                for (slot, value) in next.iter_mut().zip(component) {
                    *slot -= projection * value;
                }
            }
            normalize(&mut next);
            if dot(&next, &v).abs() > 0.999999 {
                v = next;
                break;
            }
            v = next;
        }
        components.push(v);
    }
    components
}

fn project_out(vector: &[f32], mean: &[f32], components: &[Vec<f32>], centre: bool) -> Vec<f32> {
    let mut out: Vec<f32> = vector
        .iter()
        .zip(mean)
        .map(|(x, m)| if centre { x - m } else { *x })
        .collect();
    for component in components {
        let projection = dot(&out, component);
        for (slot, value) in out.iter_mut().zip(component) {
            *slot -= projection * value;
        }
    }
    normalize(&mut out);
    out
}

fn rank_of(scored: &[(f32, String)], expected: &str) -> usize {
    scored.iter().position(|(_, name)| name == expected).map(|p| p + 1).unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let queries: Vec<String> = std::env::args().skip(1).collect();
    let queries = if queries.is_empty() {
        vec!["两个人站着".to_string(), "两个小孩".to_string()]
    } else {
        queries
    };

    let dir = model_dir();
    unsafe {
        std::env::set_var("HF_HOME", &dir);
        std::env::set_var("HF_ENDPOINT", "https://hf-mirror.com");
    }
    let device: DeviceRequest = std::env::var("SHIGUANG_DEVICE")
        .unwrap_or_else(|_| "auto".into())
        .parse()?;
    let dtype = std::env::var("SHIGUANG_DTYPE")
        .ok()
        .map(|value| embeddinggemma_core::parse_precision(&value))
        .transpose()?;
    let options = EmbedderOptions {
        device,
        dtype,
        modalities: Some(Modalities { image: true, audio: false }),
        image_soft_tokens: Some(
            std::env::var("SHIGUANG_SOFT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(140),
        ),
        ..Default::default()
    };
    let source = resolve_source("440m", &dir);
    let files = ModelFiles::locate(&source)?;
    let model = Embedder::load_files(&files, &source, "embeddinggemma-2".into(), options)?;
    model.warmup()?;
    eprintln!("model loaded from {}", dir.display());

    let version = std::env::var("SHIGUANG_VERSION")
        .unwrap_or_else(|_| "embeddinggemma-2:440m:768:original-image-v2".to_string());
    let db = Connection::open_with_flags(
        database_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut stmt = db.prepare(
        "SELECT a.asset_key,a.name,a.source,e.vector FROM embeddings e JOIN assets a USING(asset_key)
         WHERE e.model_version=?1 AND a.deleted_at IS NULL",
    )?;
    let photos: Vec<(String, String, String, Vec<f32>)> = stmt
        .query_map(params![version], |row| {
            let key: String = row.get(0)?;
            let name: String = row.get(1)?;
            let source: String = row.get(2)?;
            let bytes: Vec<u8> = row.get(3)?;
            Ok((
                key,
                name,
                source,
                bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect(),
            ))
        })?
        .collect::<Result<_, _>>()?;
    println!("version {version}: {} vectors", photos.len());

    if let Ok(path) = std::env::var("SHIGUANG_DUMP") {
        let mut out = String::from("{");
        let mut first = true;
        for query in &queries {
            let vector = model
                .embed_request(&EmbeddingsRequest {
                    input: query.clone().into(),
                    task: Some(EmbeddingTask::Query),
                    ..Default::default()
                })?
                .output
                .remove(0);
            let values: Vec<String> = vector.iter().map(|v| format!("{v:.6}")).collect();
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&format!("\n {json:?}: [{}]", values.join(","), json = query));
        }
        if let Ok(list) = std::env::var("SHIGUANG_DUMP_IMAGES") {
            for image in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let bytes = std::fs::read(image)?;
                let vector = model
                    .embed_contents(&ContentEmbeddingsRequest {
                        input: OneOrMany::One(Content::Image { data: bytes.into(), format: None }),
                        ..Default::default()
                    })?
                    .output
                    .remove(0);
                let values: Vec<String> = vector.iter().map(|v| format!("{v:.6}")).collect();
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&format!("\n {json:?}: [{}]", values.join(","), json = image));
            }
        }
        out.push_str("\n}");
        std::fs::write(&path, out)?;
        eprintln!("dumped vectors to {path}");
        return Ok(());
    }

    if std::env::var("SHIGUANG_THUMBS_EVAL").is_ok() {
        let dir = if std::env::var("SHIGUANG_SYNTH").is_ok() {
            "/tmp/sg_probe/synth".to_string()
        } else {
            std::env::var("SHIGUANG_THUMBS_DIR").unwrap_or_else(|_| "/tmp/sg_probe/thumbs".into())
        };
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .map(|e| e == "jpg" || e == "png")
                    .unwrap_or(false)
            })
            .collect();
        files.sort();
        let mut indexed: Vec<(String, Vec<f32>)> = Vec::new();
        for path in &files {
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let bytes = std::fs::read(path)?;
            let vector = model
                .embed_contents(&ContentEmbeddingsRequest {
                    input: OneOrMany::One(Content::Image { data: bytes.into(), format: None }),
                    ..Default::default()
                })?
                .output
                .remove(0);
            indexed.push((name, vector));
        }
        let synth_cases: [(&str, &str); 8] = [
            ("红圈", "circle"),
            ("a red circle on a white background", "circle"),
            ("黑色的方块", "textblock"),
            ("black text on a white background", "textblock"),
            ("绿色", "green"),
            ("绿色图片", "green"),
            ("两个小孩站着", "circle"),
            ("a vertical colourful gradient", "gradient"),
        ];
        let photo_cases: [(&str, &str); 9] = [
            ("两个小孩站着", "011"),
            ("两个不锈钢方盘放在桌上", "074"),
            ("停在车位里的汽车", "060"),
            ("校门口聚集的人群", "043"),
            ("举着手机拍到的发票截图", "022"),
            ("two children standing", "011"),
            ("两个小孩并排站在墙前面", "011"),
            ("two young children standing side by side in front of a white wall", "011"),
            ("three people standing together in a park", "027"),
        ];
        let cases: Vec<(&str, &str)> = if std::env::var("SHIGUANG_SYNTH").is_ok() {
            synth_cases.to_vec()
        } else {
            photo_cases.to_vec()
        };
        println!("\n== thumbs eval ({} images) ==", indexed.len());
        if std::env::var("SHIGUANG_IMAGE_PREFIX").is_ok() {
            // Some checkpoints only produce aligned image vectors when the
            // instruction text travels with the image in the same sequence.
            let prefixes = ["", "title: none | text: ", "task: search result | query: "];
            let mut variants: Vec<Vec<(String, Vec<f32>)>> = Vec::new();
            for prefix in prefixes {
                let mut index = Vec::new();
                for path in &files {
                    let name = path.file_stem().unwrap().to_string_lossy().to_string();
                    let bytes = std::fs::read(path)?;
                    let item = Content::Parts {
                        parts: vec![
                            Content::Text { text: prefix.to_string() },
                            Content::Image { data: bytes.into(), format: None },
                        ],
                    };
                    let vector = model
                        .embed_contents(&ContentEmbeddingsRequest {
                            input: OneOrMany::One(item),
                            ..Default::default()
                        })?
                        .output
                        .remove(0);
                    index.push((name, vector));
                }
                variants.push(index);
            }
            println!("== image + instruction prefix (rank of expected) ==");
            for (query, expected) in &cases {
                let vector = model
                    .embed_request(&EmbeddingsRequest {
                        input: query.to_string().into(),
                        task: Some(EmbeddingTask::Query),
                        ..Default::default()
                    })?
                    .output
                    .remove(0);
                let ranks: Vec<String> = variants
                    .iter()
                    .map(|index| {
                        let mut scored: Vec<(f32, String)> = index
                            .iter()
                            .map(|(name, vec)| (cosine(&vector, vec), name.clone()))
                            .collect();
                        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
                        let rank = rank_of(&scored, expected);
                        format!("{rank:>3}({:.3})", scored.first().map(|s| s.0).unwrap_or(0.0))
                    })
                    .collect();
                println!("  {query:<44} target {expected:<10} ranks(none|doc|query): {}", ranks.join(" "));
            }
            return Ok(());
        }

        if !std::env::var("SHIGUANG_QUERY_PROMPTS").is_ok() && std::env::var("SHIGUANG_VERBOSE").is_ok() {
            for (query, _) in &cases {
                let vector = model
                    .embed_request(&EmbeddingsRequest {
                        input: query.to_string().into(),
                        task: Some(EmbeddingTask::Query),
                        ..Default::default()
                    })?
                    .output
                    .remove(0);
                let row: Vec<String> = indexed
                    .iter()
                    .map(|(name, vec)| format!("{name} {:.3}", cosine(&vector, vec)))
                    .collect();
                println!("  {query:<44} {}", row.join(" | "));
            }
        }
        if std::env::var("SHIGUANG_QUERY_PROMPTS").is_ok() {
            println!("== query-side prefix variants (rank of expected photo in plain cosine) ==");
            for (query, expected) in cases {
                let mut ranks = Vec::new();
                for prompt in [
                    Some("task: search result | query: ".to_string()),
                    Some("title: none | text: ".to_string()),
                    Some(String::new()),
                ] {
                    let vector = model
                        .embed_request(&EmbeddingsRequest {
                            input: query.to_string().into(),
                            prompt: prompt.clone(),
                            task: if prompt.is_none() { Some(EmbeddingTask::Query) } else { None },
                            ..Default::default()
                        })?
                        .output
                        .remove(0);
                    let mut scored: Vec<(f32, String)> = indexed
                        .iter()
                        .map(|(name, vec)| (cosine(&vector, vec), name.clone()))
                        .collect();
                    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
                    ranks.push(rank_of(&scored, expected));
                }
                println!(
                    "  {query:<64} target {expected:<6} ranks(query|doc|none): {} {} {}",
                    ranks[0], ranks[1], ranks[2]
                );
            }
            return Ok(());
        }

        let dim = indexed[0].1.len();
        let mut mean = vec![0.0f32; dim];
        for (_, vector) in &indexed {
            for (slot, value) in mean.iter_mut().zip(vector) {
                *slot += value;
            }
        }
        for slot in mean.iter_mut() {
            *slot /= indexed.len() as f32;
        }
        let rows: Vec<Vec<f32>> = indexed.iter().map(|(_, v)| v.clone()).collect();
        let directions_1 = principal_directions(&rows, 1, &mean);
        let directions_4 = principal_directions(&rows, 4, &mean);
        let directions_8 = principal_directions(&rows, 8, &mean);
        println!("== variants: raw | abtt1 | abtt4 | abtt8 (rank of expected photo) ==");
        for (query, expected) in cases {
            let vector = model
                .embed_request(&EmbeddingsRequest {
                    input: query.to_string().into(),
                    task: Some(EmbeddingTask::Query),
                    ..Default::default()
                })?
                .output
                .remove(0);
            let mut ranks = Vec::new();
            let mut top_raw = String::new();
            for (index, directions) in [
                Vec::new(),
                directions_1.clone(),
                directions_4.clone(),
                directions_8.clone(),
            ]
            .into_iter()
            .enumerate()
            {
                let query_vector = project_out(&vector, &mean, &directions, true);
                let mut scored: Vec<(f32, String)> = indexed
                    .iter()
                    .map(|(name, vec)| {
                        let candidate = project_out(vec, &mean, &directions, true);
                        (cosine(&query_vector, &candidate), name.clone())
                    })
                    .collect();
                scored.sort_by(|a, b| b.0.total_cmp(&a.0));
                ranks.push(rank_of(&scored, expected));
                if index == 0 {
                    top_raw = scored.iter().take(3).map(|(s, n)| format!("{n} {s:.3}")).collect::<Vec<_>>().join(" | ");
                }
            }
            println!(
                "  {query:<64} target {expected:<6} ranks: {:>3} {:>3} {:>3} {:>3}   raw top: {top_raw}",
                ranks[0], ranks[1], ranks[2], ranks[3]
            );
        }
        return Ok(());
    }

    // How similar are the stored image vectors to each other?
    let mut pairs = Vec::new();
    for i in 0..photos.len() {
        for j in (i + 1)..photos.len() {
            pairs.push(cosine(&photos[i].3, &photos[j].3));
        }
    }
    pairs.sort_by(f32::total_cmp);
    let norms: Vec<f32> = photos.iter().map(|(_, _, _, v)| v.iter().map(|x| x * x).sum::<f32>().sqrt()).collect();
    println!(
        "image-image cosine: min {:.3} p50 {:.3} max {:.3} | norm min {:.3} max {:.3}",
        pairs.first().copied().unwrap_or(0.0),
        pairs[pairs.len() / 2],
        pairs.last().copied().unwrap_or(0.0),
        norms.iter().cloned().fold(f32::INFINITY, f32::min),
        norms.iter().cloned().fold(0.0, f32::max),
    );

    // Text-text sanity: neighbours should score above unrelated phrases.
    let text_probe = ["两个人站着", "两个小孩站着", "一张发票", "invoice", "两只猫"];
    let mut text_vectors = Vec::new();
    for probe in text_probe {
        let vector = model
            .embed_request(&EmbeddingsRequest {
                input: probe.to_string().into(),
                task: Some(EmbeddingTask::Query),
                ..Default::default()
            })?
            .output
            .remove(0);
        text_vectors.push(vector);
    }
    println!("\n== text-text ==");
    for i in 0..text_probe.len() {
        for j in (i + 1)..text_probe.len() {
            println!("  {:.3}  {} / {}", cosine(&text_vectors[i], &text_vectors[j]), text_probe[i], text_probe[j]);
        }
    }

    if std::env::var("SHIGUANG_VARIANTS").is_ok() {
        let files = [
            "/tmp/sg_probe/red.jpg",
            "/tmp/sg_probe/blue.jpg",
            "/tmp/sg_probe/stripes.jpg",
        ];
        let photos_to_check = ["/tmp/sg_probe/thumbs/022.jpg", "/tmp/sg_probe/thumbs/011.jpg"];
        let variants: [(&str, Option<String>); 4] = [
            ("none (app)", None),
            ("title: none | text: ", Some("title: none | text: ".into())),
            ("task: search result | query: ", Some("task: search result | query: ".into())),
            ("empty prefix", Some(String::new())),
        ];
        println!("\n== prompt variants ==");
        for (label, prompt) in variants {
            let embed = |path: &str| -> Result<Vec<f32>, Box<dyn std::error::Error>> {
                let bytes = std::fs::read(path)?;
                Ok(model
                    .embed_contents(&ContentEmbeddingsRequest {
                        input: OneOrMany::One(Content::Image { data: bytes.into(), format: None }),
                        prompt: prompt.clone(),
                        ..Default::default()
                    })?
                    .output
                    .remove(0))
            };
            let mut vectors = Vec::new();
            for path in files.iter().chain(photos_to_check.iter()) {
                vectors.push(embed(path)?);
            }
            let red_blue = cosine(&vectors[0], &vectors[1]);
            let red_stripes = cosine(&vectors[0], &vectors[2]);
            let receipt_vs_child = cosine(&vectors[3], &vectors[4]);
            let text = |q: &str| -> Result<Vec<f32>, Box<dyn std::error::Error>> {
                Ok(model
                    .embed_request(&EmbeddingsRequest {
                        input: q.to_string().into(),
                        task: Some(EmbeddingTask::Query),
                        ..Default::default()
                    })?
                    .output
                    .remove(0))
            };
            let invoice = text("一张发票")?;
            let people = text("两个人站着")?;
            println!(
                "  {label:<32} red/blue {red_blue:+.3} red/stripes {red_stripes:+.3} | receipt-vs-child {receipt_vs_child:+.3} | receipt→发票 {:.3} receipt→站着 {:.3} | child→站着 {:.3} child→发票 {:.3}",
                cosine(&vectors[3], &invoice),
                cosine(&vectors[3], &people),
                cosine(&vectors[4], &people),
                cosine(&vectors[4], &invoice),
            );
        }
    }

    // Is a freshly encoded image close to its own stored vector?
    if let Ok(list) = std::env::var("SHIGUANG_IMAGES") {
        let paths: Vec<&str> = list.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
        let mut vectors = Vec::new();
        for path in &paths {
            let bytes = std::fs::read(path)?;
            let vector = model
                .embed_contents(&ContentEmbeddingsRequest {
                    input: OneOrMany::One(Content::Image { data: bytes.into(), format: None }),
                    ..Default::default()
                })?
                .output
                .remove(0);
            vectors.push(vector);
        }
        println!("\n== synthetic image matrix ==");
        for i in 0..paths.len() {
            let row: Vec<String> = (0..paths.len())
                .map(|j| format!("{:+.3}", cosine(&vectors[i], &vectors[j])))
                .collect();
            let norm = vectors[i].iter().map(|x| x * x).sum::<f32>().sqrt();
            println!("  {:<22} norm {norm:.2}  {}", paths[i], row.join(" "));
        }
        let mut against_stored: Vec<(f32, String)> = photos
            .iter()
            .map(|(_, name, _, v)| (cosine(&vectors[0], v), name.clone()))
            .collect();
        against_stored.sort_by(|a, b| b.0.total_cmp(&a.0));
        println!(
            "  first synthetic vs stored: top {:.3} {} | bottom {:.3} {}",
            against_stored[0].0,
            against_stored[0].1,
            against_stored.last().unwrap().0,
            against_stored.last().unwrap().1
        );
    }

    if let Ok(path) = std::env::var("SHIGUANG_IMAGE") {
        let key = std::env::var("SHIGUANG_IMAGE_KEY").unwrap_or_default();
        let bytes = std::fs::read(&path)?;
        let vector = model
            .embed_contents(&ContentEmbeddingsRequest {
                input: OneOrMany::One(Content::Image { data: bytes.into(), format: None }),
                ..Default::default()
            })?
            .output
            .remove(0);
        let mut scores: Vec<(f32, String)> = photos
            .iter()
            .map(|(k, name, _, v)| (cosine(&vector, v), format!("{name} ({k})")))
            .collect();
        scores.sort_by(|a, b| b.0.total_cmp(&a.0));
        println!("\n== fresh image embedding {path} ==");
        println!("  norm {:.3}", vector.iter().map(|x| x * x).sum::<f32>().sqrt());
        for (score, label) in scores.iter().take(5) {
            let mark = if label.contains(&key) { " <= stored target" } else { "" };
            println!("  {score:.3}  {label}{mark}");
        }
        if let Some((score, _)) = scores.iter().find(|(_, label)| label.contains(&key)) {
            println!("  stored vector of the same photo scores {score:.3}");
        }
    }

    for query in &queries {
        let vector = model
            .embed_request(&EmbeddingsRequest {
                input: query.clone().into(),
                task: Some(EmbeddingTask::Query),
                ..Default::default()
            })?
            .output
            .remove(0);
        let mut scored: Vec<(f32, &str, &str, &str)> = photos
            .iter()
            .map(|(key, name, source, vec)| (cosine(&vector, vec), key.as_str(), name.as_str(), source.as_str()))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let count = scored.len().max(1) as f64;
        let mean = scored.iter().map(|s| s.0 as f64).sum::<f64>() / count;
        let std = (scored.iter().map(|s| (s.0 as f64 - mean).powi(2)).sum::<f64>() / count).sqrt();
        println!("\n== {query} == mean {mean:.3} std {std:.3}");
        if std::env::var("SHIGUANG_ALL").is_ok() {
            println!("ALL");
            for (score, _key, name, source) in scored.iter() {
                println!("{score:.4}\t{name}\t{source}");
            }
            continue;
        }
        for (score, _key, name, source) in scored.iter().take(8) {
            let z = (score - mean as f32) / std.max(0.02) as f32;
            println!("  {score:.3}  z={z:+.2}  {name} [{source}]");
        }
        let tail = scored.iter().rev().take(3).fold(String::new(), |acc, (score, _, name, _)| {
            format!("{acc} {score:.3}:{name}")
        });
        println!("  weakest:{tail}");
    }
    Ok(())
}
