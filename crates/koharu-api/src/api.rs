use anyhow::{Context as _, Result};

use super::handlers;

pub async fn run(host: String, port: u16, cpu: bool) -> Result<()> {
    koharu_ml::init()
        .await
        .context("failed to initialize the ML runtime")?;
    let device = koharu_ml::device(cpu);
    koharu_metrics::context(serde_json::json!({
        "compute_backend": device.backend.to_string().to_ascii_lowercase(),
        "device_type": format!("{:?}", device.device_type).to_ascii_lowercase(),
        "gpu_model": device.description.clone(),
        "vram_bytes": device.memory_total,
    }));

    let pipeline = koharu_pipeline::Pipeline::load(device)?;

    let app = handlers::router(pipeline)?;

    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
