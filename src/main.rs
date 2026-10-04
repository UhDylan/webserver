use axum::{
    extract::{Path, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use tokio::fs;

#[derive(Deserialize)]
struct MediaParams {
    media_type: String,
    id: String,
}

#[derive(Deserialize)]
struct DirectParams {
    file: String,
}

fn get_cache_dir() -> PathBuf {
    std::env::current_dir()
        .map(|p| if p.ends_with("src") { p.parent().unwrap().to_path_buf() } else { p })
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("cache")
}

#[tokio::main]
async fn main() {
    let _ = fs::create_dir_all(get_cache_dir()).await;

    let app = Router::new()
        .route("/{media_type}/{id}", get(proxy_ifunny))
        .route("/direct/{file}", get(serve_direct_media));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .unwrap();

    println!("[Axum] Server running on http://{}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn proxy_ifunny(
    Path(params): Path<MediaParams>,
    _request: Request,
) -> Response {
    let clean_id = params.id.split('?').next().unwrap_or(&params.id);
    let target_url = format!("https://ifunny.co/{}/{}", params.media_type, clean_id);

    Redirect::temporary(&target_url).into_response()
}

async fn serve_direct_media(Path(params): Path<DirectParams>) -> Response {
    let clean_file = params.file.split('?').next().unwrap_or(&params.file);
    
    let is_mp4_request = clean_file.ends_with(".mp4");
    let target_ext = if is_mp4_request { "mp4" } else { "gif" };

    let clean_id = match clean_file.rfind('.') {
        Some(idx) => &clean_file[..idx],
        None => clean_file,
    };

    let cache_dir = get_cache_dir();
    let cache_file_path = cache_dir.join(format!("{}.{}", clean_id, target_ext));

    // 1. Serve cached media immediately if available
    if cache_file_path.exists() {
        if let Ok(bytes) = fs::read(&cache_file_path).await {
            println!("[Axum] Serving cached {} for {}", target_ext, clean_id);
            let content_type = if is_mp4_request { "video/mp4" } else { "image/gif" };
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
            headers.insert(header::CACHE_CONTROL, "public, max-age=31536000".parse().unwrap());
            return (StatusCode::OK, headers, bytes).into_response();
        }
    }

    // 2. Fetch direct CDN URL directly from iFunny API
    let client = reqwest::Client::new();
    let media_url = match fetch_ifunny_cdn_url(&client, clean_id).await {
        Some(url) => {
            println!("[Axum] Resolved CDN URL for {}: {}", clean_id, url);
            url
        }
        None => {
            println!("[Axum] ERROR: Could not resolve CDN URL for ID {}", clean_id);
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    // 3. Download source media from CDN
    match client.get(&media_url).header(USER_AGENT, "Mozilla/5.0").send().await {
        Ok(resp) => {
            if let Ok(bytes) = resp.bytes().await {
                // If MP4 was requested directly
                if is_mp4_request {
                    println!("[Axum] Serving original MP4 video for {}", clean_id);
                    let _ = fs::write(&cache_file_path, &bytes).await;

                    let mut headers = HeaderMap::new();
                    headers.insert(header::CONTENT_TYPE, "video/mp4".parse().unwrap());
                    headers.insert(header::CACHE_CONTROL, "public, max-age=31536000".parse().unwrap());
                    return (StatusCode::OK, headers, bytes).into_response();
                }

                // Prepare file for GIF conversion
                let is_video = media_url.contains(".mp4") || media_url.contains("/videos/");
                let temp_ext = if is_video { "mp4" } else { "jpg" };

                let temp_input_path = cache_dir.join(format!("temp_{}.{}", clean_id, temp_ext));
                let _ = fs::write(&temp_input_path, &bytes).await;

                let input_file = temp_input_path.clone();
                let gif_out = cache_file_path.clone();

                println!("[Axum] Converting downloaded media to GIF for {}", clean_id);

                let conversion_result = tokio::task::spawn_blocking(move || {
                    let mut cmd = Command::new("ffmpeg");
                    cmd.arg("-y");

                    if is_video {
                        cmd.args([
                            "-i", input_file.to_str().unwrap(),
                            "-vf", "fps=12,scale=360:-1:flags=lanczos",
                            "-loop", "0",
                            gif_out.to_str().unwrap(),
                        ]);
                    } else {
                        cmd.args([
                            "-loop", "1",
                            "-i", input_file.to_str().unwrap(),
                            "-vf", "fps=1,scale=360:-1:flags=lanczos",
                            "-t", "1",
                            gif_out.to_str().unwrap(),
                        ]);
                    }

                    cmd.output()
                })
                .await;

                let _ = fs::remove_file(&temp_input_path).await;

                match conversion_result {
                    Ok(Ok(output)) => {
                        if output.status.success() {
                            println!("[Axum] FFmpeg GIF conversion succeeded for {}", clean_id);
                            if let Ok(gif_bytes) = fs::read(&cache_file_path).await {
                                let mut headers = HeaderMap::new();
                                headers.insert(header::CONTENT_TYPE, "image/gif".parse().unwrap());
                                headers.insert(header::CACHE_CONTROL, "public, max-age=31536000".parse().unwrap());
                                return (StatusCode::OK, headers, gif_bytes).into_response();
                            }
                        } else {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            println!("[Axum] ERROR: FFmpeg conversion failed:\n{}", stderr);
                        }
                    }
                    Ok(Err(e)) => println!("[Axum] ERROR: Failed to run FFmpeg: {:?}", e),
                    Err(e) => println!("[Axum] ERROR: Join error during conversion: {:?}", e),
                }
            }
        }
        Err(e) => println!("[Axum] ERROR: Failed to download source media: {:?}", e),
    }

    StatusCode::BAD_GATEWAY.into_response()
}

async fn fetch_ifunny_cdn_url(client: &reqwest::Client, id: &str) -> Option<String> {
    let api_url = format!("https://ifunny.co/api/v1/content/{}", id);

    let response = client
        .get(&api_url)
        .header(USER_AGENT, "iFunny/7.16.1(1124451) Android/11")
        .header(ACCEPT, "application/json")
        .send()
        .await
        .ok()?;

    if response.status().is_success() {
        let json: Value = response.json().await.ok()?;
        
        // 1. Try video URL
        if let Some(url) = json.pointer("/result/video_url").and_then(|v| v.as_str()) {
            return Some(url.to_string());
        }

        // 2. Try GIF/picture URL
        if let Some(url) = json.pointer("/result/url").and_then(|v| v.as_str()) {
            return Some(url.to_string());
        }
    }

    // Fallback: Construct deterministic CDN stream URL from hash
    for ext in ["mp4", "gif", "jpg"] {
        let direct_url = format!("https://img.ifunny.co/videos/{}_1.{}", id, ext);
        let check = client.head(&direct_url).send().await;
        if let Ok(res) = check {
            if res.status().is_success() {
                return Some(direct_url);
            }
        }
    }

    None
}
