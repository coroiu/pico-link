//! `HttpServer`: the headless-mode testability protocol — lets an agent
//! (or a human with `curl`) inject `NavIntent`s and pull a screenshot over
//! a real TCP socket, with no window and no hardware. Also serves in
//! windowed mode (where `/api/screenshot` just 404s, since there's no
//! `HeadlessSurface` to read from), so a shutdown request works uniformly
//! either way.
//!
//! Endpoints:
//! - `POST /api/input`: enqueues a `NavIntent` (JSON) for a headless
//!   `HttpInput` to drain on its next poll.
//! - `GET /api/screenshot`: PNG of the framebuffer most recently flushed
//!   to the registered `HeadlessSurface` (headless mode only; 404
//!   otherwise).
//! - `POST /api/shutdown`: signals the render loop to stop.

use crate::platform::HeadlessSurface;
use pico_link_core::input::NavIntent;
use std::collections::VecDeque;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server, StatusCode};

pub struct HttpServer {
    server: Server,
    should_shutdown: Arc<AtomicBool>,
    /// Fed by `POST /api/input`, drained by a headless
    /// `emulator::platform::HttpInput` on every render-loop poll.
    input_queue: Arc<Mutex<VecDeque<NavIntent>>>,
    /// The `HeadlessSurface` `GET /api/screenshot` reads from, if any.
    /// `None` in windowed mode (there is no `HeadlessSurface` to read);
    /// set via `set_screenshot_surface` before the server starts handling
    /// requests in headless mode.
    screenshot_surface: Option<Arc<Mutex<HeadlessSurface>>>,
}

impl HttpServer {
    /// # Errors
    ///
    /// Returns an error if the underlying `tiny_http::Server` could not
    /// bind `addr` (e.g. the port is already in use).
    pub fn new(addr: &str) -> Result<Self, Box<dyn Error>> {
        let server = Server::http(addr).map_err(|e| format!("Failed to start server: {e}"))?;

        Ok(Self {
            server,
            should_shutdown: Arc::new(AtomicBool::new(false)),
            input_queue: Arc::new(Mutex::new(VecDeque::new())),
            screenshot_surface: None,
        })
    }

    #[must_use]
    pub fn get_shutdown_signal(&self) -> Arc<AtomicBool> {
        self.should_shutdown.clone()
    }

    /// Hands out the shared queue `POST /api/input` enqueues `NavIntent`s
    /// into. A headless `emulator::platform::HttpInput` holds the other
    /// end, draining it on every `InputSource::poll()`.
    #[must_use]
    pub fn get_input_queue_ref(&self) -> Arc<Mutex<VecDeque<NavIntent>>> {
        self.input_queue.clone()
    }

    /// Registers the `HeadlessSurface` handle `GET /api/screenshot` reads
    /// from. Only meaningful in headless mode; windowed mode never calls
    /// this, so `/api/screenshot` responds 404 there. Must be called
    /// before the server starts handling requests (i.e. before moving the
    /// `HttpServer` into its request-loop thread) — there is no
    /// synchronization protecting concurrent calls to this method itself.
    pub fn set_screenshot_surface(&mut self, surface: Arc<Mutex<HeadlessSurface>>) {
        self.screenshot_surface = Some(surface);
    }

    /// The address the server actually bound to. Useful when binding to
    /// port 0 (an ephemeral port), e.g. in tests that don't want to
    /// hardcode/collide on 8080.
    ///
    /// # Panics
    ///
    /// Panics if the underlying listener isn't a TCP socket (`tiny_http`
    /// also supports Unix sockets, which this project never binds to —
    /// `new` is always called with an `ip:port` string).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.server.server_addr().to_ip().expect("HttpServer always binds a TCP address, never a Unix socket")
    }

    /// # Errors
    ///
    /// Returns an error if receiving or responding to the request fails
    /// at the TCP layer.
    pub fn handle_request(&self) -> Result<(), Box<dyn Error>> {
        let request = self.server.recv()?;

        match (request.method(), request.url()) {
            (&Method::Post, "/api/input") => self.handle_input(request),
            (&Method::Get, "/api/screenshot") => self.handle_screenshot(request),
            (&Method::Post, "/api/shutdown") => self.handle_shutdown(request),
            _ => request
                .respond(Response::from_string("Not Found").with_status_code(StatusCode(404)))
                .map_err(Into::into),
        }
    }

    /// `POST /api/input`: enqueues a `NavIntent` for a headless
    /// `HttpInput` to drain on its next poll — the agent-drivable input
    /// half of the headless testability protocol. Body is the JSON form
    /// of `pico_link_core::input::NavIntent`'s derived `Deserialize`: a bare
    /// string for the unit variants (e.g. `"Next"`, `"Prev"`,
    /// `"Activate"`, `"Back"`) or `{"NextN":5}` for the one tuple variant.
    fn handle_input(&self, mut request: tiny_http::Request) -> Result<(), Box<dyn Error>> {
        let intent: NavIntent = serde_json::from_reader(request.as_reader())?;
        self.input_queue.lock().unwrap().push_back(intent);

        let response = serde_json::json!({
            "status": "success",
            "queued": format!("{intent:?}"),
        });

        request
            .respond(Response::from_string(response.to_string()).with_header("Content-Type: application/json".parse::<Header>().unwrap()))
            .map_err(Into::into)
    }

    /// `GET /api/screenshot`: PNG-encodes the framebuffer most recently
    /// flushed to the registered `HeadlessSurface` (see
    /// `set_screenshot_surface`) — the observable half of the headless
    /// testability protocol, paired with `handle_input` above. Responds
    /// 404 if no surface is registered (not running headless) and 503 if
    /// a surface is registered but nothing has been rendered yet.
    fn handle_screenshot(&self, request: tiny_http::Request) -> Result<(), Box<dyn Error>> {
        let Some(surface) = &self.screenshot_surface else {
            return request
                .respond(
                    Response::from_string("Screenshot unavailable: not running in headless mode")
                        .with_status_code(StatusCode(404)),
                )
                .map_err(Into::into);
        };

        match surface.lock().unwrap().encode_png() {
            Some(png_bytes) => request
                .respond(Response::from_data(png_bytes).with_header("Content-Type: image/png".parse::<Header>().unwrap()))
                .map_err(Into::into),
            None => request
                .respond(Response::from_string("No frame rendered yet").with_status_code(StatusCode(503)))
                .map_err(Into::into),
        }
    }

    fn handle_shutdown(&self, request: tiny_http::Request) -> Result<(), Box<dyn Error>> {
        self.should_shutdown.store(true, Ordering::Relaxed);

        let response = serde_json::json!({
            "status": "success",
            "message": "Shutting down emulator",
        });

        request
            .respond(Response::from_string(response.to_string()).with_header("Content-Type: application/json".parse::<Header>().unwrap()))
            .map_err(Into::into)
    }
}
