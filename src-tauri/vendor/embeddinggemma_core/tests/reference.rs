//! End-to-end checks against `transformers` (float32, eager attention).
//!
//! Produce the reference with `scripts/reference.py` (needs torch,
//! torchvision and transformers ≥ 5.19), then:
//!
//! ```text
//! python -I scripts/reference.py models/embeddinggemma-2 target/reference
//! EMBEDDINGGEMMA_REFERENCE_DIR=target/reference cargo test --release -p embeddinggemma_core --test reference -- --ignored --nocapture
//! EMBEDDINGGEMMA_TEST_DEVICE=cuda EMBEDDINGGEMMA_REFERENCE_DIR=… cargo test --release --features cuda -p embeddinggemma_core --test reference -- --ignored --nocapture
//! ```
//!
//! The weights come from `./models` (or the Hub). bf16 and accelerators are
//! held to looser cosine bounds than f32 on the CPU.

use embeddinggemma_core::{DeviceRequest, Embedder, EmbedderOptions, Part, Precision};
use serde_json::Value;
use std::path::PathBuf;

fn reference_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("EMBEDDINGGEMMA_REFERENCE_DIR")
            .expect("set EMBEDDINGGEMMA_REFERENCE_DIR to the output of scripts/reference.py"),
    )
}

fn load() -> Embedder {
    let device: DeviceRequest = std::env::var("EMBEDDINGGEMMA_TEST_DEVICE")
        .unwrap_or_else(|_| "cpu".into())
        .parse()
        .unwrap();
    let dtype = std::env::var("EMBEDDINGGEMMA_TEST_DTYPE")
        .ok()
        .map(|d| embeddinggemma_core::parse_precision(&d).unwrap());
    let dir = std::env::var("EMBEDDINGGEMMA_MODEL_DIR").unwrap_or_else(|_| "../../models".into());
    Embedder::load_from(
        "embeddinggemma-2",
        std::path::Path::new(&dir),
        EmbedderOptions {
            device,
            dtype,
            ..Default::default()
        },
    )
    .unwrap()
}

fn vectors(v: &Value) -> Vec<Vec<f32>> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap() as f32)
                .collect()
        })
        .collect()
}

fn ids(v: &Value) -> Vec<u32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_u64().unwrap() as u32)
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

/// Minimum cosine for a check: exact kernels in f32, rounding noise in bf16.
fn bound(e: &Embedder, f32_bound: f32) -> f32 {
    if e.info().dtype == Precision::F32 {
        f32_bound
    } else {
        0.995
    }
}

fn report(name: &str, got: &[f32], want: &[f32], min_cos: f32) {
    let c = cosine(got, want);
    eprintln!(
        "{name:<28} cosine {c:.6}  max|Δ| {:.2e}  (bound {min_cos})",
        max_diff(got, want)
    );
    assert!(c >= min_cos, "{name}: cosine {c} < {min_cos}");
}

#[test]
#[ignore = "needs the weights and scripts/reference.py output"]
fn matches_transformers() {
    let dir = reference_dir();
    let r: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("reference.json")).unwrap())
            .unwrap();
    let e = load();
    let info = e.info();
    eprintln!(
        "{} on {} / {} ({})",
        info.qualified_id, info.device, info.dtype, info.backend
    );

    // Text: token ids exactly, vectors batched and alone.
    let texts: Vec<String> = r["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    for (t, want) in texts.iter().zip(r["text_ids"].as_array().unwrap()) {
        assert_eq!(e.tokenize(t).unwrap(), ids(want), "token ids of {t:?}");
    }
    let got = e.embed_texts(&texts, true).unwrap().vectors;
    for (i, (g, w)) in got.iter().zip(vectors(&r["text"])).enumerate() {
        report(&format!("text[{i}] (batched)"), g, &w, bound(&e, 0.99999));
    }
    for (i, (t, w)) in texts.iter().zip(vectors(&r["text_single"])).enumerate() {
        let g = e
            .embed_texts(std::slice::from_ref(t), true)
            .unwrap()
            .vectors
            .remove(0);
        report(&format!("text[{i}] (alone)"), &g, &w, bound(&e, 0.99999));
    }

    // Image: placeholder layout exactly, pixels approximately (resampler),
    // vector closely.
    let png = std::fs::read(dir.join("test.png")).unwrap();
    let image = embeddinggemma_core::media::image::decode_image(&png).unwrap();
    let (row, counts) = e.prepare_row(&[Part::Image(image.clone())]).unwrap();
    assert_eq!(row.ids, ids(&r["image_ids"]), "image token layout");
    eprintln!("image soft tokens {}", counts.image);
    let g = e
        .embed_inputs(&[vec![Part::Image(image.clone())]], None, true)
        .unwrap()
        .vectors
        .remove(0);
    report("image", &g, &vectors(&r["image"])[0], bound(&e, 0.999));
    // Already at a target size: no resampling, so the tower must be exact.
    let exact = embeddinggemma_core::media::image::decode_image(
        &std::fs::read(dir.join("test768.png")).unwrap(),
    )
    .unwrap();
    let g = e
        .embed_inputs(&[vec![Part::Image(exact)]], None, true)
        .unwrap()
        .vectors
        .remove(0);
    report(
        "image (no resize)",
        &g,
        &vectors(&r["image_exact"])[0],
        bound(&e, 0.99999),
    );

    // Text + image in one vector.
    let parts = vec![
        Part::Text("a photo of a red circle ".into()),
        Part::Image(image),
    ];
    let (row, _) = e.prepare_row(&parts).unwrap();
    assert_eq!(
        row.ids.len(),
        r["mixed_len"].as_u64().unwrap() as usize,
        "mixed length"
    );
    assert_eq!(
        &row.ids[..12],
        ids(&r["mixed_ids_head"]).as_slice(),
        "mixed ids"
    );
    let g = e
        .embed_inputs(&[parts], None, true)
        .unwrap()
        .vectors
        .remove(0);
    report(
        "text + image",
        &g,
        &vectors(&r["mixed"])[0],
        bound(&e, 0.999),
    );

    // Audio: WAV decode, mel features, token layout, vector.
    let wav = std::fs::read(dir.join("test.wav")).unwrap();
    let samples = e.decode_audio(&wav, Some("wav")).unwrap();
    let (row, counts) = e.prepare_row(&[Part::Audio(samples.clone())]).unwrap();
    assert_eq!(row.ids, ids(&r["audio_ids"]), "audio token layout");
    eprintln!("audio soft tokens {}", counts.audio);
    let cfg = embeddinggemma_core::config::ProcessorConfig::default();
    let mel = embeddinggemma_core::media::audio::log_mel(&samples, &cfg).unwrap();
    assert_eq!(mel.frames, r["audio_frames"].as_u64().unwrap() as usize);
    assert_eq!(
        mel.valid_frames,
        r["audio_valid_frames"].as_u64().unwrap() as usize
    );
    let want_head = vectors(&r["audio_mel_head"]);
    for (f, w) in want_head.iter().enumerate() {
        let got = &mel.mel[f * 128..f * 128 + 8];
        assert!(max_diff(got, w) < 1e-3, "mel frame {f}: {got:?} vs {w:?}");
    }
    let mid: Vec<f32> = r["audio_mel_mid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() as f32)
        .collect();
    assert!(
        max_diff(&mel.mel[100 * 128 + 40..100 * 128 + 48], &mid) < 1e-3,
        "mel frame 100"
    );
    let g = e
        .embed_inputs(&[vec![Part::Audio(samples)]], None, true)
        .unwrap()
        .vectors
        .remove(0);
    report("audio", &g, &vectors(&r["audio"])[0], bound(&e, 0.9999));
}
