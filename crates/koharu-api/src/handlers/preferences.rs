use axum::{
    Json,
    extract::{self, State},
};
use koharu_renderer::{FontFamily, Renderer};
use koharu_translator::Model;

use crate::preferences::{self, Preferences};

use super::ApiResult;

pub(crate) async fn get_preferences() -> ApiResult<Json<Preferences>> {
    Ok(Json(Preferences::load()?))
}

pub(crate) async fn get_translation_models() -> ApiResult<Json<Vec<Model>>> {
    Ok(Json(koharu_translator::Translator::models().await?))
}

pub(crate) async fn save_preferences(
    extract::Json(preferences): extract::Json<Preferences>,
) -> ApiResult<()> {
    preferences::save_preferences(preferences).await?;
    Ok(())
}

pub(crate) async fn list_fonts(
    State(renderer): State<Renderer>,
) -> ApiResult<Json<Vec<FontFamily>>> {
    Ok(Json(
        renderer
            .available_fonts()
            .await?
            .into_iter()
            .map(FontFamily::from)
            .collect(),
    ))
}
