//! Embed texts, images, audio, videos and PDFs from the command line and
//! print the pairwise cosine scores (or the vectors as JSON).
//!
//! ```text
//! cargo run --release -p embeddinggemma_core --example embed -- \
//!     --task query "a red bicycle" --image bike.jpg --audio bell.wav
//! cargo run --release --features cuda -p embeddinggemma_core --example embed -- --device cuda --json "text"
//! cargo run --release --features mlx  -p embeddinggemma_core --example embed -- --device mlx "text"
//! ```
//!
//! Every argument that is not an option is one text input; `--image`,
//! `--audio`, `--video` and `--pdf` each add one media input (file path).
//! Texts get the `--task` prompt (default `document`); media get none.

use embeddinggemma_core::{
    Content, ContentEmbeddingsRequest, DeviceRequest, Embedder, EmbedderOptions, EmbeddingTask,
};
use std::time::Instant;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = "embeddinggemma-2".to_string();
    let mut device: DeviceRequest = std::env::var("EMBEDDINGGEMMA_DEVICE")
        .unwrap_or_default()
        .parse()?;
    let mut dtype = std::env::var("EMBEDDINGGEMMA_DTYPE").ok();
    let mut task: Option<String> = None;
    let mut dimensions: Option<u32> = None;
    let mut json = false;
    let mut items: Vec<(String, Content)> = Vec::new();

    let read = |p: &str| std::fs::read(p).map_err(|e| format!("{p}: {e}"));
    let label = |p: &str| {
        std::path::Path::new(p)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| p.to_string())
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--model" | "-m" => model = value("--model")?,
            "--device" | "-d" => device = value("--device")?.parse()?,
            "--dtype" => dtype = Some(value("--dtype")?),
            "--task" | "-t" => task = Some(value("--task")?),
            "--dimensions" => dimensions = Some(value("--dimensions")?.parse()?),
            "--json" => json = true,
            "--image" => {
                let p = value("--image")?;
                items.push((
                    label(&p),
                    Content::Image {
                        data: read(&p)?.into(),
                        format: None,
                    },
                ));
            }
            "--audio" => {
                let p = value("--audio")?;
                let format = p.rsplit('.').next().map(str::to_string);
                items.push((
                    label(&p),
                    Content::Audio {
                        data: read(&p)?.into(),
                        format,
                    },
                ));
            }
            "--video" | "--pdf" => {
                let p = value(&arg)?;
                items.push((
                    label(&p),
                    Content::File {
                        data: read(&p)?.into(),
                        name: Some(p),
                    },
                ));
            }
            "--help" | "-h" => {
                println!(
                    "usage: embed [--model ID] [--device auto|cpu|cuda|metal|mlx] [--dtype f32|bf16] \
                     [--task TASK] [--dimensions N] [--json] [--image F] [--audio F] [--video F] [--pdf F] TEXT..."
                );
                return Ok(());
            }
            _ => items.push((format!("{arg:?}"), Content::text(arg))),
        }
    }
    if items.is_empty() {
        return Err("nothing to embed (try --help)".into());
    }

    let options = EmbedderOptions {
        device,
        dtype: dtype
            .as_deref()
            .map(embeddinggemma_core::parse_precision)
            .transpose()?,
        ..Default::default()
    };
    eprintln!("loading {model} on {device}…");
    let embedder = Embedder::load(&model, options)?;
    let info = embedder.info();
    eprintln!(
        "loaded {} ({}, inputs: {}, {} / {} / {}) in {:.1}s",
        info.qualified_id,
        info.parameters,
        info.modalities.inputs().join(" "),
        info.backend,
        info.device,
        info.dtype,
        info.load_seconds
    );

    let started = Instant::now();
    let labels: Vec<String> = items.iter().map(|(l, _)| l.clone()).collect();
    let response = embedder.embed_contents(&ContentEmbeddingsRequest {
        input: items.into_iter().map(|(_, c)| c).collect::<Vec<_>>().into(),
        task: task.as_deref().map(EmbeddingTask::parse),
        dimensions,
        ..Default::default()
    })?;
    eprintln!(
        "embedded {} inputs in {:.0} ms ({:?})",
        labels.len(),
        started.elapsed().as_secs_f64() * 1000.0,
        response
            .usage
            .as_ref()
            .and_then(|u| u.input.clone())
            .unwrap_or_default()
    );

    if json {
        println!("{}", serde_json::to_string(&response.output)?);
        return Ok(());
    }
    let vectors = &response.output;
    let width = labels
        .iter()
        .map(|l| l.chars().count().min(40))
        .max()
        .unwrap_or(0);
    for (i, a) in vectors.iter().enumerate() {
        let label: String = labels[i].chars().take(40).collect();
        let scores: Vec<String> = vectors
            .iter()
            .map(|b| format!("{:6.3}", a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()))
            .collect();
        println!("{label:<width$}  {}", scores.join(" "));
    }
    Ok(())
}
