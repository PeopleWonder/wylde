//! voice-npu-spike — Phase 10 NPU reachability check.
//!
//! Goal: see whether `ort` 2.x with the OpenVINO EP can load a Whisper
//! ONNX encoder, target Intel NPU, apply the `[1, 80, 3000]` static
//! reshape (the same workaround Voice/transcribe.py:258 uses on its
//! OpenVINO IR), and run inference.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use ort::ep::{ArbitrarilyConfigurableExecutionProvider, ExecutionProvider, OpenVINO};
use ort::inputs;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::TensorRef;

#[derive(Parser, Debug)]
#[command(about = "Whisper-on-NPU spike via ort + OpenVINO EP")]
struct Args {
    /// Path to the encoder ONNX file.
    #[arg(long)]
    encoder: PathBuf,

    /// OpenVINO device target. NPU | CPU | GPU | "HETERO:NPU,CPU".
    #[arg(long, default_value = "NPU")]
    device: String,

    /// Number of warmup runs before timed run.
    #[arg(long, default_value_t = 1)]
    warmup: usize,

    /// Number of timed runs to average.
    #[arg(long, default_value_t = 3)]
    runs: usize,

    /// Skip the OpenVINO EP — useful to capture a CPU baseline number
    /// from the same binary.
    #[arg(long)]
    cpu_only: bool,
}

fn main() -> ort::Result<()> {
    let args = Args::parse();

    println!("=== voice-npu-spike ===");
    println!("encoder    : {}", args.encoder.display());
    println!("device     : {}", args.device);
    println!("cpu_only   : {}", args.cpu_only);
    println!();

    println!(
        "OpenVINO EP supported_by_platform = {}",
        OpenVINO::default().supported_by_platform()
    );
    let is_avail = OpenVINO::default().is_available();
    println!("OpenVINO EP is_available           = {:?}", is_avail);
    println!();

    let mut builder = Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_intra_threads(4)?;

    if !args.cpu_only {
        let ep = OpenVINO::default()
            .with_device_type(&args.device)
            // Replicates Voice/transcribe.py:278 enc_model.reshape({"input_features": [1, 80, 3000]}).
            .with_arbitrary_config("reshape_input", "input_features[1,80,3000]")
            // VPUX needs static shapes.
            .with_dynamic_shapes(false)
            .with_cache_dir("./ov_cache")
            .build()
            .error_on_failure();
        builder = builder.with_execution_providers([ep])?;
    }

    let t0 = Instant::now();
    let mut session = builder.commit_from_file(&args.encoder)?;
    let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("Encoder session loaded in {:.1} ms", load_ms);

    println!("Inputs:");
    for input in session.inputs() {
        println!("  {}", input.name());
    }
    println!("Outputs:");
    for output in session.outputs() {
        println!("  {}", output.name());
    }
    println!();

    // Whisper encoder input: log-mel spectrogram [1, 80, 3000] f32.
    // Pure-zero input is a valid shape/dtype sanity check; we don't
    // need real audio to validate the *NPU compilation* path.
    let n = 1usize * 80 * 3000;
    let data = vec![0.0f32; n];
    let shape: Vec<i64> = vec![1, 80, 3000];

    // Warmup.
    for i in 0..args.warmup {
        let input = TensorRef::from_array_view((shape.clone(), data.as_slice()))?;
        let t = Instant::now();
        let outputs = session.run(inputs!["input_features" => input])?;
        let _ = outputs;
        let elapsed = t.elapsed().as_secs_f64() * 1000.0;
        println!("Warmup {}: {:.1} ms", i + 1, elapsed);
    }

    // Timed runs.
    let mut samples = Vec::with_capacity(args.runs);
    for i in 0..args.runs {
        let input = TensorRef::from_array_view((shape.clone(), data.as_slice()))?;
        let t = Instant::now();
        let outputs = session.run(inputs!["input_features" => input])?;
        let first = outputs
            .iter()
            .next()
            .expect("encoder produced at least one output")
            .1;
        let (out_shape, _out_data) = first.try_extract_tensor::<f32>()?;
        let shape_vec: Vec<i64> = out_shape.to_vec();
        let elapsed = t.elapsed().as_secs_f64() * 1000.0;
        samples.push(elapsed);
        println!(
            "Run {}: {:.1} ms  out_shape={:?}",
            i + 1,
            elapsed,
            shape_vec
        );
    }

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let min = samples[0];
    let max = samples[samples.len() - 1];
    println!();
    println!("=== Encoder-only summary ({} runs) ===", samples.len());
    println!("min/median/max: {:.1} / {:.1} / {:.1} ms", min, median, max);

    Ok(())
}
