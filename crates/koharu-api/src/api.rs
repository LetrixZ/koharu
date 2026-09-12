use anyhow::{Context as _, Result};

use crate::handlers;

pub async fn run() -> Result<()> {
    koharu_ml::init()
        .await
        .context("failed to initialize the ML runtime")?;
    let device = koharu_ml::device(false);
    koharu_metrics::context(serde_json::json!({
        "compute_backend": device.backend.to_string().to_ascii_lowercase(),
        "device_type": format!("{:?}", device.device_type).to_ascii_lowercase(),
        "gpu_model": device.description.clone(),
        "vram_bytes": device.memory_total,
    }));

    let pipeline = koharu_pipeline::Pipeline::load(device)?;

    let app = handlers::router(pipeline)?;

    // TODO: Get address from config
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8148").await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
