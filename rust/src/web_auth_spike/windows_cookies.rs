use super::SpikeSource;
pub(super) use crate::windows_webview::{clear, set_test_cookie};
use wry::WebView;

pub(super) fn read(
    view: &WebView,
    source: SpikeSource,
) -> Result<Vec<crate::windows_webview::CapturedCookie>, &'static str> {
    let domains: &[&str] = match source {
        SpikeSource::AnimePictures => &[
            "anime-pictures.net",
            "api.anime-pictures.net",
            "opreviews.anime-pictures.net",
            "oimages.anime-pictures.net",
        ],
        SpikeSource::KonachanCom => &["konachan.com", "www.konachan.com"],
    };
    crate::windows_webview::read(
        view,
        &domains
            .iter()
            .map(|domain| (*domain).into())
            .collect::<Vec<_>>(),
    )
}
