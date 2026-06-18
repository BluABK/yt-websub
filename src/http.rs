//! Top-level request dispatch: callback routes (`/yt/cb/<token>`) and
//! token-authenticated control routes (`/api/*`) share the one HTTPS listener.

use tiny_http::{Request, Response};

use crate::app::App;

pub fn handle(app: &App, req: Request) {
    let url = req.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url, String::new()),
    };

    if let Some(token) = path.strip_prefix("/yt/cb/") {
        let token = token.trim_end_matches('/').to_string();
        crate::callback::handle(app, req, &token, &query);
    } else if path.starts_with("/api/") {
        crate::api::handle(app, req, &path, &query);
    } else {
        let _ = req.respond(Response::empty(404u16));
    }
}
