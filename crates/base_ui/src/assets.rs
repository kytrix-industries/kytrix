use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }

        match Self::get(path).map(|f| Some(f.data)) {
            Some(data) => Ok(data),
            None => {
                println!("Asset not found: {}", path);
                Ok(None)
            }
        }
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect())
    }
}
