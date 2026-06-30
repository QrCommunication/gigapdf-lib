//! Persistent OCR microservice: load the PaddleOCR-on-RTen models **once** at boot and serve
//! recognition over a minimal local HTTP/1.1 endpoint. Zero web-framework dependency — just
//! `std::net` + the crate's own `image` decoder. A host (e.g. the Next.js server) renders a PDF
//! page to PNG and POSTs the bytes; the service returns the recognized words.
//!
//! This is the "host-side endpoint" the lean wasm client is meant to call: it amortizes the
//! ~hundreds-of-MB model load across every request instead of paying it per invocation.
//!
//! Usage: `ocr_serve <models_dir> [bind_addr]`
//!   - `models_dir` (or env `OCR_MODELS_DIR`): the `load_models_dir` layout
//!     (`det.rten` + `<lang>/{model.rten,dict.txt}`).
//!   - `bind_addr`  (or env `OCR_BIND`): default `127.0.0.1:8077`.
//!
//! Endpoints:
//!   - `GET  /health`        → `{"ok":true,"recCount":N,"languages":[...]}`
//!   - `POST /ocr` body=PNG  → NDJSON, one object per recognized line, in **image pixel space**
//!     (top-left origin): `{"text","x","y","w","h","confidence","model"}`.
//!     Optional header `X-Ocr-Model: <name>` forces a recognizer (e.g. `latin_hw` for
//!     handwriting, or a specific script); omit / `auto` ⇒ automatic per-line script selection.
//!
//! Single-threaded by design: OCR is CPU-bound, so requests are served sequentially (one heavy
//! inference at a time) — no shared-state locking, no thread-safety assumptions on the models.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use gigapdf_ocr_rten::OcrEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let models_dir = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("OCR_MODELS_DIR").ok())
        .ok_or("usage: ocr_serve <models_dir> [bind_addr]   (or set OCR_MODELS_DIR)")?;
    let bind = args
        .get(2)
        .cloned()
        .or_else(|| std::env::var("OCR_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1:8077".to_string());

    eprintln!("ocr_serve: loading models from {models_dir} …");
    let engine = OcrEngine::load_models_dir(&models_dir)?;
    eprintln!(
        "ocr_serve: {} rec model(s) loaded [{}]; listening on http://{bind}",
        engine.rec_count(),
        engine.rec_names().join(", ")
    );

    let listener = TcpListener::bind(&bind)?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(e) = handle(stream, &engine) {
                    eprintln!("ocr_serve: connection error: {e}");
                }
            }
            Err(e) => eprintln!("ocr_serve: accept error: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, engine: &OcrEngine) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream.try_clone()?);

    // ── Request line: "METHOD PATH HTTP/1.1"
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // client closed
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // ── Headers (until the blank line). Cap line count to bound a hostile client.
    let mut content_length = 0usize;
    let mut model: Option<String> = None;
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = lower.strip_prefix("x-ocr-model:") {
            let m = v.trim().to_string();
            if !m.is_empty() && m != "auto" {
                model = Some(m);
            }
        }
    }

    if method == "GET" && path.starts_with("/health") {
        let languages = engine
            .rec_names()
            .iter()
            .map(|s| json_str(s))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(
            "{{\"ok\":true,\"recCount\":{},\"languages\":[{languages}]}}",
            engine.rec_count()
        );
        return write_response(&mut stream, 200, "application/json", body.as_bytes());
    }

    if method == "POST" && path.starts_with("/ocr") {
        // Bound the body to a sane page-image size (64 MB) to avoid a memory-exhaustion request.
        if content_length == 0 || content_length > 64 * 1024 * 1024 {
            return write_response(&mut stream, 400, "text/plain", b"missing/oversized body");
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;

        let img = match image::load_from_memory(&body) {
            Ok(i) => i.to_rgb8(),
            Err(e) => {
                return write_response(
                    &mut stream,
                    400,
                    "text/plain",
                    format!("bad image: {e}").as_bytes(),
                )
            }
        };

        let recognized = match &model {
            Some(name) => engine.recognize_page_with(&img, name),
            None => engine.recognize_page(&img),
        };
        let lines = match recognized {
            Ok(lines) => lines,
            Err(e) => {
                return write_response(
                    &mut stream,
                    500,
                    "text/plain",
                    format!("ocr error: {e}").as_bytes(),
                )
            }
        };

        // NDJSON in **image pixel space** (top-left origin) — the host's existing geometry code
        // converts these to PDF user space.
        let mut ndjson = String::new();
        for l in &lines {
            ndjson.push_str(&format!(
                "{{\"text\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{},\"confidence\":{:.4},\"model\":{}}}\n",
                json_str(&l.text),
                l.bbox.x0,
                l.bbox.y0,
                l.bbox.x1.saturating_sub(l.bbox.x0),
                l.bbox.y1.saturating_sub(l.bbox.y0),
                l.confidence,
                json_str(&l.model),
            ));
        }
        return write_response(&mut stream, 200, "application/x-ndjson", ndjson.as_bytes());
    }

    write_response(&mut stream, 404, "text/plain", b"not found")
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// Minimal JSON string literal (quoted + escaped) for the `text`/`model` fields.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
