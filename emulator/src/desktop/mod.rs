// The HTTP server that lets an agent (or a human with `curl`) drive the
// headless shell over a real TCP socket, and observe it via a screenshot
// — see `http_server`'s own doc comment for the endpoint list.

pub mod http_server;

pub use http_server::HttpServer;
