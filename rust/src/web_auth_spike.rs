//! Opt-in Windows compatibility harness. It never imports into the running app.
//! Cookie material stays in memory; only fixed stage names and error codes are reported.

use crate::api::booru_auth::WebCookie;
use crate::api::booru_models::{
    AuthKind, BooruEngineConfig, ContentRating, MediaVariantKind, NetworkFallbackMode,
    RouteProvider,
};
use crate::booru::adapter::{BooruAdapter, RequestContext};
use crate::booru::adapters::anime_pictures::AnimePicturesAdapter;
use crate::booru::adapters::moebooru::MoebooruAdapter;
use crate::booru::auth::SessionStore;
use crate::booru::error::BooruError;
use crate::booru::media::download::download_to_path;
use crate::booru::media::request::prepared_download;
use crate::booru::model::SearchQuery;
use crate::booru::network::{NetworkRuntime, RequestSpec, ResponseKind};
use crate::booru::source::{builtin_sources, SourceDefinition};
use muda::{Menu, MenuEvent, MenuItem, Submenu};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::WindowExtWindows;
use tao::window::WindowBuilder;
use tempfile::TempDir;
use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

pub mod transactions;
mod windows_cookies;

const SOURCE: &str = "anime_pictures";
const ENTRY: &str = "https://anime-pictures.net/";
const ACCOUNT: &str = "https://api.anime-pictures.net/api/v3/users/me";
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const SESSION_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpikeSource {
    AnimePictures,
    KonachanCom,
}

impl SpikeSource {
    fn entry(self) -> &'static str {
        match self {
            Self::AnimePictures => ENTRY,
            Self::KonachanCom => "https://konachan.com/",
        }
    }

    fn probe_url(self) -> &'static str {
        match self {
            Self::AnimePictures => ACCOUNT,
            Self::KonachanCom => "https://konachan.com/post.json?limit=1&tags=rating%3As",
        }
    }

    fn definition(self) -> SourceDefinition {
        let original = match self {
            Self::AnimePictures => SOURCE,
            Self::KonachanCom => "konachan_net",
        };
        let mut source = builtin_sources()
            .into_iter()
            .find(|source| source.id == original)
            .unwrap();
        if self == Self::KonachanCom {
            // Only this isolated harness uses .com. Do not silently change the registered .net source.
            source.id = "konachan_com_spike";
            source.display_name = "Konachan.com (compatibility spike)";
            source.api_base = "https://konachan.com";
            source.web_base = "https://konachan.com";
            source.referer = "https://konachan.com/";
            source.allowed_hosts = &["konachan.com", "www.konachan.com"];
            source.allowed_host_suffixes = &["konachan.com"];
        }
        source.capabilities.auth_kind = AuthKind::WebSession;
        source
    }
}

// No Debug implementation: these events may contain credentials.
enum UiEvent {
    Verify,
    OpenAccount,
    Close,
    Capture { attempt: u64, ua_json: String },
    Status { attempt: u64, text: String },
    Finished { attempt: u64 },
}

/// Launch the visible, user-driven compatibility harness on the Windows main thread.
pub fn run() {
    // A panic must never print a Cookie or a WebView evaluation result.
    std::panic::set_hook(Box::new(|_| eprintln!("web-auth-spike: internal error")));
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = if args == ["--self-test"] {
        run_self_test()
    } else if args.is_empty() {
        run_window(SpikeSource::AnimePictures)
    } else if args == ["--source", "konachan.com"] {
        run_window(SpikeSource::KonachanCom)
    } else {
        Err("usage: web_auth_spike [--self-test | --source konachan.com]")
    };
    if let Err(stage) = result {
        eprintln!("web-auth-spike: {stage}");
        std::process::exit(1);
    }
}

fn run_window(source: SpikeSource) -> Result<(), &'static str> {
    let profile = tempfile::Builder::new()
        .prefix("booruflow-web-auth-spike-")
        .tempdir()
        .map_err(|_| "temporary profile unavailable")?;
    let mut context = WebContext::new(Some(profile.path().join("webview")));
    let mut event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("BooruFlow 会话测试 — 手动登录后点击「会话测试 → 验证」")
        .with_inner_size(tao::dpi::LogicalSize::new(1100.0, 800.0))
        .build(&event_loop)
        .map_err(|_| "window unavailable")?;
    let verify = MenuItem::new("验证并测试 Rust 请求", true, None);
    let open_account = MenuItem::new("打开当前图源 API 页面（手动验证）", true, None);
    let close = MenuItem::new("取消并关闭", true, None);
    let menu = Menu::with_items(&[&Submenu::with_items(
        "会话测试",
        true,
        &[&verify, &open_account, &close],
    )
    .map_err(|_| "menu unavailable")?])
    .map_err(|_| "menu unavailable")?;
    // SAFETY: the HWND belongs to this live Tao window on the current UI thread.
    unsafe { menu.init_for_hwnd(window.hwnd()) }.map_err(|_| "menu unavailable")?;
    let verify_id = verify.id().clone();
    let account_id = open_account.id().clone();
    let close_id = close.id().clone();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let command = if event.id == verify_id {
            UiEvent::Verify
        } else if event.id == account_id {
            UiEvent::OpenAccount
        } else if event.id == close_id {
            UiEvent::Close
        } else {
            return;
        };
        let _ = menu_proxy.send_event(command);
    }));
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(source.entry())
        .with_devtools(false)
        .with_navigation_handler(move |url| navigation_allowed_for(source, &url))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .build(&window)
        .map_err(|_| "WebView2 unavailable; install or update Microsoft Edge WebView2 Runtime")?;
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "worker unavailable")?;
    let mut webview = Some(webview);
    let mut worker: Option<tokio::task::JoinHandle<()>> = None;
    let mut busy = false;
    let mut attempt = 0_u64;
    let deadline = Instant::now() + SESSION_TIMEOUT;
    let mut capture_deadline = None;

    println!(
        "web-auth-spike: ready (Windows, source={}, system proxy, fallback off)",
        source.definition().id
    );
    println!("请手动完成登录/验证，然后点击窗口菜单：会话测试 → 验证并测试 Rust 请求。");
    println!("结果仅用于兼容性测试，不保存或导入主应用会话。窗口 10 分钟后自动关闭。");
    event_loop.run_return(|event, _, control_flow| {
        // Keep these alive until the event loop ends.
        let _keep_alive = (&menu, &context, &runtime);
        *control_flow = ControlFlow::WaitUntil(
            capture_deadline.map_or(deadline, |capture: Instant| capture.min(deadline)),
        );
        if Instant::now() >= deadline {
            println!("web-auth-spike: timedOut");
            *control_flow = ControlFlow::Exit;
            return;
        }
        if capture_deadline.is_some_and(|time| Instant::now() >= time) {
            capture_deadline = None;
            attempt = attempt.wrapping_add(1);
            busy = false;
            verify.set_enabled(true);
            window.set_title("BooruFlow 会话测试 — UA 读取超时，可重试");
        }
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
            | Event::UserEvent(UiEvent::Close) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UiEvent::OpenAccount) if !busy => {
                if let Some(view) = &webview {
                    let _ = view.load_url(source.probe_url());
                }
            }
            Event::UserEvent(UiEvent::Verify) if !busy => {
                if let Some(view) = &webview {
                    attempt = attempt.wrapping_add(1);
                    let capture_attempt = attempt;
                    let callback_proxy = proxy.clone();
                    busy = true;
                    verify.set_enabled(false);
                    capture_deadline = Some(Instant::now() + Duration::from_secs(15));
                    window.set_title("BooruFlow 会话测试 — 正在采集");
                    if view
                        .evaluate_script_with_callback("navigator.userAgent", move |ua_json| {
                            let _ = callback_proxy.send_event(UiEvent::Capture {
                                attempt: capture_attempt,
                                ua_json,
                            });
                        })
                        .is_err()
                    {
                        busy = false;
                        capture_deadline = None;
                        verify.set_enabled(true);
                        window.set_title("BooruFlow 会话测试 — UA 读取失败，可重试");
                    }
                }
            }
            Event::UserEvent(UiEvent::Capture {
                attempt: captured,
                ua_json,
            }) if busy && captured == attempt && capture_deadline.is_some() => {
                capture_deadline = None;
                let captured = webview
                    .as_ref()
                    .ok_or("window closed")
                    .and_then(|view| capture(view, &ua_json, source));
                match captured {
                    Ok((cookies, ua)) => {
                        let worker_proxy = proxy.clone();
                        let support = profile.path().join("runtime");
                        worker = Some(runtime.spawn(async move {
                            let timeout = tokio::time::timeout(
                                Duration::from_secs(180),
                                probe(cookies, ua, support, attempt, worker_proxy.clone(), source),
                            )
                            .await;
                            if timeout.is_err() {
                                report(&worker_proxy, attempt, "请求超时".to_owned());
                            }
                            let _ = worker_proxy.send_event(UiEvent::Finished { attempt });
                        }));
                    }
                    Err(code) => {
                        busy = false;
                        verify.set_enabled(true);
                        println!("web-auth-spike: capture rejected ({code})");
                        window.set_title(&format!("BooruFlow 会话测试 — 采集失败：{code}"));
                    }
                }
            }
            Event::UserEvent(UiEvent::Status {
                attempt: reported,
                text,
            }) if reported == attempt => {
                println!("web-auth-spike: {text}");
                window.set_title(&format!("BooruFlow 会话测试 — {text}"));
            }
            Event::UserEvent(UiEvent::Finished { attempt: finished }) if finished == attempt => {
                busy = false;
                verify.set_enabled(true);
                worker = None;
            }
            _ => {}
        }
    });
    if let Some(task) = worker.take() {
        task.abort();
        let _ = runtime.block_on(task);
    }
    close_webview(&mut webview);
    drop(context);
    drop(window);
    cleanup_profile(profile);
    Ok(())
}

fn close_webview(webview: &mut Option<WebView>) {
    if let Some(view) = webview.take() {
        if windows_cookies::clear(&view).is_err() {
            eprintln!("web-auth-spike: profile clear failed");
        }
        drop(view);
    }
}

fn cleanup_profile(profile: TempDir) {
    // This path came directly from our TempDir, never from a page or IPC payload.
    let path = profile.path().to_owned();
    if profile.close().is_ok() {
        return;
    }
    // WebView2's child process may release its files after its controller closes.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        match std::fs::remove_dir_all(&path) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {}
        }
    }
    eprintln!("web-auth-spike: profile cleanup pending");
}

#[cfg(test)]
fn navigation_allowed(value: String) -> bool {
    navigation_allowed_for(SpikeSource::AnimePictures, &value)
}

fn navigation_allowed_for(source: SpikeSource, value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && match source {
                SpikeSource::AnimePictures => matches!(
                    url.host_str(),
                    Some("anime-pictures.net" | "api.anime-pictures.net")
                ),
                SpikeSource::KonachanCom => {
                    matches!(url.host_str(), Some("konachan.com" | "www.konachan.com"))
                }
            }
    })
}

fn capture(
    view: &WebView,
    ua_json: &str,
    source: SpikeSource,
) -> Result<(Vec<WebCookie>, String), &'static str> {
    let ua: String = serde_json::from_str(ua_json).map_err(|_| "invalidUserAgent")?;
    if ua.is_empty() || ua.len() > 4096 || reqwest::header::HeaderValue::from_str(&ua).is_err() {
        return Err("invalidUserAgent");
    }
    if !navigation_allowed_for(source, &view.url().map_err(|_| "invalidFinalUrl")?) {
        return Err("invalidFinalUrl");
    }
    let captured = windows_cookies::read(view, source)?
        .into_iter()
        .map(|item| item.material)
        .collect::<Vec<_>>();
    if captured.is_empty() {
        return Err("noCookiesCaptured");
    }
    Ok((captured, ua))
}

#[cfg(test)]
fn cookie_domain_allowed(domain: &str) -> bool {
    cookie_domain_allowed_for(SpikeSource::AnimePictures, domain)
}

fn cookie_domain_allowed_for(source: SpikeSource, domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    if source == SpikeSource::KonachanCom {
        return matches!(
            domain.strip_prefix('.').unwrap_or(&domain),
            "konachan.com" | "www.konachan.com"
        );
    }
    matches!(
        domain.strip_prefix('.').unwrap_or(&domain),
        "anime-pictures.net"
            | "api.anime-pictures.net"
            | "opreviews.anime-pictures.net"
            | "oimages.anime-pictures.net"
    )
}

/// Local WebView2 capability test. It does not load or solve a remote challenge.
fn run_self_test() -> Result<(), &'static str> {
    let profile = tempfile::Builder::new()
        .prefix("booruflow-web-auth-self-test-")
        .tempdir()
        .map_err(|_| "selfTestProfileUnavailable")?;
    let mut context = WebContext::new(Some(profile.path().join("webview")));
    let event_loop = EventLoopBuilder::<()>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_visible(false)
        .build(&event_loop)
        .map_err(|_| "selfTestWindowUnavailable")?;
    let view = WebViewBuilder::new_with_web_context(&mut context)
        .with_html("<!doctype html><title>BooruFlow local WebView2 self-test</title>")
        .with_devtools(false)
        .build(&window)
        .map_err(|_| "selfTestWebView2Unavailable")?;
    windows_cookies::set_test_cookie(&view)?;
    let all = windows_cookies::read(&view, SpikeSource::AnimePictures)?;
    let captured = all
        .iter()
        .find(|item| item.material.name == "spike_http_only")
        .ok_or("selfTestHttpOnlyCookieMissing")?;
    let material = &captured.material;
    if material.value != "abc=def=="
        || material.path != "/api/v3"
        || material.domain != ".anime-pictures.net"
        || !captured.http_only
        || !material.secure
        || material.expires_at_iso8601.is_none()
    {
        return Err("selfTestCookieMetadataMismatch");
    }
    let scoped = view
        .cookies_for_url(ACCOUNT)
        .map_err(|_| "selfTestScopedReadFailed")?;
    let root = view
        .cookies_for_url(ENTRY)
        .map_err(|_| "selfTestRootReadFailed")?;
    if !scoped.iter().any(|item| item.name() == "spike_http_only")
        || root.iter().any(|item| item.name() == "spike_http_only")
    {
        return Err("selfTestCookiePathMismatch");
    }
    windows_cookies::clear(&view)?;
    let deletion_deadline = Instant::now() + Duration::from_secs(2);
    while windows_cookies::read(&view, SpikeSource::AnimePictures)?
        .iter()
        .any(|item| item.material.name == "spike_http_only")
    {
        if Instant::now() >= deletion_deadline {
            return Err("selfTestCookieNotDeleted");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut view = Some(view);
    close_webview(&mut view);
    drop(context);
    drop(window);
    cleanup_profile(profile);
    println!(
        "web-auth-spike: local WebView2 self-test passed (HttpOnly, equals, expiry, path, delete)"
    );
    Ok(())
}

fn report(proxy: &EventLoopProxy<UiEvent>, attempt: u64, text: String) {
    let _ = proxy.send_event(UiEvent::Status { attempt, text });
}

fn report_error(proxy: &EventLoopProxy<UiEvent>, attempt: u64, stage: &str, error: &BooruError) {
    report(
        proxy,
        attempt,
        format!(
            "{stage}失败：{:?}（HTTP {}）",
            error.code,
            error
                .http_status
                .map_or("-".to_owned(), |status| status.to_string())
        ),
    );
}

async fn probe(
    cookies: Vec<WebCookie>,
    ua: String,
    support: PathBuf,
    attempt: u64,
    proxy: EventLoopProxy<UiEvent>,
    selected: SpikeSource,
) {
    let result = probe_inner(cookies, ua, support, attempt, &proxy, selected).await;
    if let Err((stage, error)) = result {
        report_error(&proxy, attempt, stage, &error);
    }
}

async fn probe_inner(
    cookies: Vec<WebCookie>,
    ua: String,
    support: PathBuf,
    attempt: u64,
    proxy: &EventLoopProxy<UiEvent>,
    selected: SpikeSource,
) -> Result<(), (&'static str, BooruError)> {
    let source = selected.definition();
    let definitions = vec![source.clone()];
    let sessions = Arc::new(SessionStore::default());
    sessions
        .set_web_session(source.id, cookies, ua.clone())
        .map_err(|error| ("候选校验", error))?;
    let runtime = Arc::new(
        NetworkRuntime::new(
            BooruEngineConfig {
                app_support_directory: support.to_string_lossy().into_owned(),
                proxy_url: None,
                fallback_mode: NetworkFallbackMode::Off,
                dns_provider: RouteProvider::UncensoredDns,
                user_agent: ua,
            },
            &definitions,
            sessions,
        )
        .map_err(|error| ("网络初始化", error))?,
    );
    let ctx = RequestContext { runtime };
    let adapter: Box<dyn BooruAdapter> = match selected {
        SpikeSource::AnimePictures => Box::new(AnimePicturesAdapter::new(source.clone())),
        SpikeSource::KonachanCom => Box::new(MoebooruAdapter::new(source.clone())),
    };

    let account_ok = if selected == SpikeSource::AnimePictures {
        report(proxy, attempt, "正在测试账号 API".to_owned());
        let account_response = ctx
            .runtime
            .execute(
                &source,
                RequestSpec::get(url::Url::parse(ACCOUNT).unwrap(), ResponseKind::Json),
            )
            .await;
        match account_response {
            Ok(response) => {
                let valid = account_has_identity(&response.body);
                report(
                    proxy,
                    attempt,
                    if valid {
                        "账号响应含有效身份"
                    } else {
                        "账号响应未识别为有效身份（需确认站点结构）"
                    }
                    .to_owned(),
                );
                valid
            }
            Err(error) => {
                report_error(proxy, attempt, "账号 API", &error);
                false
            }
        }
    } else {
        false
    };
    report(proxy, attempt, "正在测试列表 API".to_owned());
    let page = adapter
        .search(
            &ctx,
            SearchQuery {
                text: String::new(),
                ratings: vec![ContentRating::General],
                page_size: 1,
                cursor: None,
            },
        )
        .await
        .map_err(|error| ("列表 API", error))?;
    let post = page.items.first().ok_or_else(|| {
        (
            "没有可测试的普通分级图片",
            BooruError::invalid_response(source.id),
        )
    })?;
    report(proxy, attempt, "列表 API 通过，正在测试预览图".to_owned());
    let downloads = tempfile::tempdir()
        .map_err(|_| ("临时下载目录", BooruError::invalid_request(Some(source.id))))?;
    for (variant, label) in [
        (MediaVariantKind::Thumbnail, "预览图"),
        (MediaVariantKind::Original, "原图"),
    ] {
        let prepared = adapter
            .prepare_media(&ctx, &post.id, variant)
            .await
            .and_then(|media| prepared_download(source.id, &post.id.0, variant, media))
            .map_err(|error| (label, error))?;
        download_to_path(
            &ctx.runtime,
            &source,
            &prepared,
            &downloads.path().join(&prepared.filename),
            |progress| {
                if progress.received > MAX_DOWNLOAD_BYTES
                    || progress.total.is_some_and(|size| size > MAX_DOWNLOAD_BYTES)
                {
                    return Err(BooruError::invalid_request(Some(source.id)));
                }
                Ok(())
            },
        )
        .await
        .map_err(|error| (label, error))?;
        report(proxy, attempt, format!("{label}完整下载校验通过"));
    }
    report(
        proxy,
        attempt,
        if selected == SpikeSource::KonachanCom {
            "Konachan.com 本次 API/预览/原图探针通过；需确认是否实际经历 CF 挑战"
        } else if account_ok {
            "本次账号/API/预览/原图探针通过；尚未验证持久化和重启"
        } else {
            "API/预览/原图通过；账号身份仍未确认"
        }
        .to_owned(),
    );
    Ok(())
}

// Spike only: conservative candidate schemas; the real response must be confirmed
// before changing the production adapter's account-validation contract.
fn account_has_identity(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    if value
        .get("error")
        .is_some_and(|error| !error.is_null() && error != false)
        || value.get("success") == Some(&Value::Bool(false))
    {
        return false;
    }
    let user = value.get("user").unwrap_or(&value);
    user.get("id")
        .and_then(Value::as_u64)
        .is_some_and(|id| id > 0)
        && user
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn konachan_spike_is_scoped_to_the_requested_com_site() {
        let source = SpikeSource::KonachanCom;
        assert!(navigation_allowed_for(source, "https://konachan.com/post"));
        assert!(!navigation_allowed_for(source, "https://konachan.net/post"));
        assert!(!navigation_allowed_for(
            source,
            "https://konachan.com.attacker.test/"
        ));
        assert!(cookie_domain_allowed_for(source, ".konachan.com"));
        assert!(!cookie_domain_allowed_for(source, ".com"));
        assert!(!cookie_domain_allowed_for(source, ".anime-pictures.net"));
        assert_eq!(
            source.definition().capabilities.auth_kind,
            AuthKind::WebSession
        );
        let builtin = builtin_sources()
            .into_iter()
            .find(|source| source.id == "konachan_net")
            .unwrap();
        assert_eq!(builtin.api_base, "https://konachan.net");
        assert_eq!(builtin.capabilities.auth_kind, AuthKind::None);
    }

    #[test]
    fn navigation_is_https_and_exact_host_only() {
        assert!(navigation_allowed(ENTRY.to_owned()));
        for url in [
            "http://anime-pictures.net/",
            "https://anime-pictures.net:8443/",
            "https://anime-pictures.net.attacker.test/",
            "https://evil-anime-pictures.net/",
            "https://user@anime-pictures.net/",
            "file:///test",
        ] {
            assert!(!navigation_allowed(url.to_owned()), "{url}");
        }
    }

    #[test]
    fn cookie_import_does_not_accept_arbitrary_parent_domains() {
        assert!(cookie_domain_allowed(".anime-pictures.net"));
        assert!(cookie_domain_allowed("api.anime-pictures.net"));
        for domain in [
            "net",
            ".net",
            "evil.anime-pictures.net",
            "anime-pictures.net.attacker.test",
            "..anime-pictures.net",
        ] {
            assert!(!cookie_domain_allowed(domain));
        }
    }

    #[test]
    fn empty_or_business_error_json_is_not_an_account() {
        for body in [
            "{}",
            "null",
            "[]",
            r#"{"user":{"id":0,"name":"anonymous"}}"#,
            r#"{"success":false,"user":{"id":1,"name":"test"}}"#,
            r#"{"error":"denied","id":1,"name":"test"}"#,
        ] {
            assert!(!account_has_identity(body.as_bytes()));
        }
        assert!(account_has_identity(br#"{"user":{"id":1,"name":"test"}}"#));
    }
}
