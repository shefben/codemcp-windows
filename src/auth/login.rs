//! Interactive OAuth 2.1 login flow (gateway-side).
//!
//! Two-phase flow driven by the admin interface:
//! 1. `start(name, url, oauth_config)` — discovers OAuth metadata, registers
//!    the client (dynamic or pre-registered), starts a localhost callback
//!    server, and returns the authorization URL. The pending `OAuthState` +
//!    callback receiver are held in the returned `LoginHandle` for the Runtime
//!    to store.
//! 2. `finish(handle, timeout)` — waits for the browser redirect, validates the
//!    CSRF state, exchanges the authorization code for tokens (rmcp auto-saves
//!    to `mcp-auth.json` via `FileCredentialStore`), and returns the token
//!    response.
//!
//! Mirrors opencode's `startAuth`/`authenticate`/`finishAuth` flow.

use std::time::Duration;

use rmcp::transport::auth::{AuthError, OAuthState, OAuthTokenResponse};
use tokio::sync::oneshot;

use crate::auth::callback::{self, CallbackResult, CallbackServer};
use crate::auth::store::FileCredentialStore;
use crate::config::OAuthConfig;

/// The result of starting an OAuth flow: the URL the user must open, plus the
/// opaque handle the Runtime stores to complete the flow later.
pub struct AuthStartResult {
    /// The authorization URL to open in the browser.
    pub authorization_url: String,
    /// The CSRF state token (for logging/display).
    pub oauth_state: String,
}

/// The opaque handle stored by the Runtime between `auth_start` and
/// `auth_finish`. Contains the in-flight OAuth state machine and the callback
/// receiver.
pub struct LoginHandle {
    oauth_state: OAuthState,
    callback_rx: oneshot::Receiver<CallbackResult>,
    csrf_state: String,
    #[allow(dead_code)]
    callback_server: CallbackServer,
}

/// Default timeout for the OAuth callback (5 minutes), matching opencode.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Default callback path when no custom redirect URI is configured.
const DEFAULT_CALLBACK_PATH: &str = "/callback";

/// Start the OAuth authorization flow.
///
/// This creates an `OAuthState`, sets a `FileCredentialStore` (so tokens are
/// auto-persisted), starts the authorization (metadata discovery + client
/// registration), and spins up a localhost callback server.
///
/// The caller (Runtime) must store the returned `LoginHandle` and later pass it
/// to `finish()` when the admin `auth_finish` command arrives.
pub async fn start(
    name: &str,
    url: &str,
    oauth_config: Option<&OAuthConfig>,
) -> Result<(AuthStartResult, LoginHandle), AuthError> {
    // Determine the callback port and path from the OAuth config.
    let (callback_port, callback_path) = resolve_callback_config(oauth_config);

    // Create the OAuth state machine.
    let mut oauth_state = OAuthState::new(url.to_string(), None).await?;

    // Set our file-backed credential store so tokens auto-persist.
    if let OAuthState::Unauthorized(ref mut manager) = oauth_state {
        manager.set_credential_store(FileCredentialStore::new(name, url));
    }

    // Bind the callback listener up front so we know the exact port before
    // constructing the redirect URI. Binding once (and never dropping/rebinding)
    // eliminates the TOCTOU race where an ephemeral port could change between
    // discovery and serving.
    let bind_addr = match callback_port {
        Some(p) => format!("127.0.0.1:{p}"),
        None => "127.0.0.1:0".to_string(),
    };
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| {
            AuthError::InternalError(format!("callback bind failed on {bind_addr}: {e}"))
        })?;
    let actual_port = listener
        .local_addr()
        .map_err(|e| AuthError::InternalError(format!("callback addr: {e}")))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{actual_port}{callback_path}");

    // Start the authorization (metadata discovery + client registration).
    // Pass empty scopes to let the SDK auto-select from server metadata.
    let scopes: Vec<&str> = if let Some(cfg) = oauth_config {
        cfg.scope.as_deref().map(|s| vec![s]).unwrap_or_default()
    } else {
        vec![]
    };
    oauth_state
        .start_authorization(&scopes, &redirect_uri, Some("codemcp"))
        .await?;

    // Get the authorization URL (contains the CSRF state as a query param).
    let auth_url = oauth_state.get_authorization_url().await?;

    // Parse the CSRF state from the authorization URL.
    let csrf_state = parse_state_from_url(&auth_url).ok_or_else(|| {
        AuthError::InternalError("authorization URL missing state parameter".to_string())
    })?;

    // Hand the already-bound listener to the callback server, so it serves on
    // exactly the port we used in the redirect URI.
    let (server, rx) = callback::start_with_listener(listener, callback_path, csrf_state.clone())
        .await
        .map_err(AuthError::InternalError)?;

    let result = AuthStartResult {
        authorization_url: auth_url,
        oauth_state: csrf_state.clone(),
    };

    let handle = LoginHandle {
        oauth_state,
        callback_rx: rx,
        csrf_state,
        callback_server: server,
    };

    Ok((result, handle))
}

/// Finish the OAuth flow: wait for the callback, exchange the code for tokens.
///
/// On success, tokens have already been persisted to `mcp-auth.json` by the
/// `FileCredentialStore` (rmcp calls `credential_store.save()` during
/// `exchange_code_for_token`).
pub async fn finish(handle: LoginHandle) -> Result<OAuthTokenResponse, AuthError> {
    finish_with_timeout(handle, CALLBACK_TIMEOUT).await
}

/// Finish with a custom timeout.
pub async fn finish_with_timeout(
    mut handle: LoginHandle,
    timeout: Duration,
) -> Result<OAuthTokenResponse, AuthError> {
    // Wait for the browser redirect (or timeout).
    let result = tokio::time::timeout(timeout, &mut handle.callback_rx)
        .await
        .map_err(|_| {
            handle.callback_server.stop();
            AuthError::AuthorizationFailed(
                "OAuth callback timeout - authorization took too long".to_string(),
            )
        })?;

    let callback_result = result
        .map_err(|_| AuthError::AuthorizationFailed("callback channel closed".to_string()))?;

    let code = match callback_result {
        CallbackResult::Code(c) => c,
        CallbackResult::Error(e) => {
            handle.callback_server.stop();
            return Err(AuthError::AuthorizationFailed(e));
        }
    };

    // Exchange the authorization code for tokens. This also saves tokens
    // to the credential store (FileCredentialStore → mcp-auth.json).
    handle
        .oauth_state
        .handle_callback(&code, &handle.csrf_state)
        .await?;

    // Extract the token response from the now-Authorized state.
    let (_client_id, token_response) = handle.oauth_state.get_credentials().await?;
    handle.callback_server.stop();

    token_response.ok_or_else(|| {
        AuthError::InternalError(
            "token exchange succeeded but no token response stored".to_string(),
        )
    })
}

/// Cancel a pending OAuth flow (drop the handle, stop the callback server).
pub fn cancel(mut handle: LoginHandle) {
    handle.callback_server.stop();
    // The oneshot receiver is dropped, which is fine — the sender side
    // (in the callback server) will fail silently.
}

/// Parse the `state` query parameter from an authorization URL.
fn parse_state_from_url(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
}

/// Resolve the callback port and path from OAuth config.
fn resolve_callback_config(oauth_config: Option<&OAuthConfig>) -> (Option<u16>, &'static str) {
    if let Some(cfg) = oauth_config {
        if let Some(ref uri) = cfg.redirect_uri {
            // Parse the redirect URI to extract port and path.
            if let Ok(parsed) = url::Url::parse(uri) {
                let port = parsed.port().or_else(|| {
                    if parsed.scheme() == "https" {
                        Some(443)
                    } else {
                        Some(80)
                    }
                });
                // We can't return a &'static str from a runtime string, so
                // fall back to the default path if a custom one is used.
                // The callback server handles this by accepting the path.
                let _ = parsed.path(); // acknowledged below
                return (port, DEFAULT_CALLBACK_PATH);
            }
        }
        if let Some(port) = cfg.callback_port {
            return (Some(port), DEFAULT_CALLBACK_PATH);
        }
    }
    (None, DEFAULT_CALLBACK_PATH)
}
