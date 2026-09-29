use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Listener, Manager, Runtime};
use tokio::sync::oneshot;

pub const REQUEST_EVENT: &str = "astrobox://frontinvoke/request";
pub const RESPONSE_EVENT: &str = "astrobox://frontinvoke/response";
const FRONTEND_READY_GENERATION_EVENT: &str = "astrobox://frontend/ready-generation";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct InvokeOptions {
    /// Stable client identity used to namespace frontend idempotency records.
    pub client_id: Option<String>,
    /// Stable caller-owned id used by the frontend bridge for idempotent work.
    pub request_id: Option<String>,
    /// Absolute UNIX time in milliseconds. The frontend rejects expired work.
    /// `None` means no deadline is enforced.
    pub deadline_at_ms: Option<u64>,
    /// Frontend queue-manager generation observed by the caller.
    pub readiness_generation: Option<u64>,
    /// Local waiter bound. This never extends `deadline_at_ms`.
    /// `None` indicates an unbounded wait (no timeout).
    pub timeout: Option<Duration>,
}

impl Default for InvokeOptions {
    fn default() -> Self {
        Self {
            client_id: None,
            request_id: None,
            deadline_at_ms: None,
            readiness_generation: None,
            timeout: Some(DEFAULT_TIMEOUT),
        }
    }
}

impl InvokeOptions {
    /// Returns options with no timeout bound and no deadline.
    pub fn infinite() -> Self {
        Self {
            client_id: None,
            request_id: None,
            deadline_at_ms: None,
            readiness_generation: None,
            timeout: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct FrontInvokeRequest {
    id: u64,
    method: String,
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "deadlineAt", skip_serializing_if = "Option::is_none")]
    deadline_at_ms: Option<u64>,
    #[serde(
        rename = "readinessGeneration",
        skip_serializing_if = "Option::is_none"
    )]
    readiness_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct FrontInvokeResponse {
    id: u64,
    success: bool,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "readinessGeneration")]
    readiness_generation: Option<u64>,
}

struct PendingRequest {
    sender: oneshot::Sender<FrontInvokeResponse>,
    readiness_generation: Option<u64>,
}

struct PendingRequestGuard {
    state: Arc<FrontInvokeState>,
    id: u64,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.state.remove_pending(self.id);
    }
}

struct FrontInvokeState {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingRequest>>,
}

impl FrontInvokeState {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn register_listener<R: Runtime>(self: &Arc<Self>, app_handle: &AppHandle<R>) {
        let response_state = Arc::clone(self);
        let _ = app_handle.listen_any(RESPONSE_EVENT, move |event| {
            let payload = event.payload();
            match serde_json::from_str::<FrontInvokeResponse>(payload) {
                Ok(resp) => response_state.resolve(resp),
                Err(err) => {
                    log::error!("[frontbridge] failed to parse response payload: {err}");
                }
            }
        });

        let generation_state = Arc::clone(self);
        let _ = app_handle.listen_any(FRONTEND_READY_GENERATION_EVENT, move |event| {
            let generation = serde_json::from_str::<serde_json::Value>(event.payload())
                .ok()
                .and_then(|payload| payload.get("generation").and_then(|value| value.as_u64()));
            if let Some(generation) = generation {
                generation_state.invalidate_generation(generation);
            }
        });
    }

    fn resolve(&self, resp: FrontInvokeResponse) {
        let pending = self
            .pending
            .lock()
            .expect("frontbridge pending map poisoned")
            .remove(&resp.id);
        if let Some(pending) = pending {
            let _ = pending.sender.send(resp);
        } else {
            log::debug!(
                "[frontbridge] late response ignored for id={}",
                resp.id
            );
        }
    }

    fn invalidate_generation(&self, generation: u64) {
        let ids = {
            let pending = self
                .pending
                .lock()
                .expect("frontbridge pending map poisoned");
            pending
                .iter()
                .filter_map(|(id, request)| {
                    (request.readiness_generation.is_some_and(|expected| expected != generation))
                        .then_some(*id)
                })
                .collect::<Vec<_>>()
        };
        for id in ids {
            let request = self
                .pending
                .lock()
                .expect("frontbridge pending map poisoned")
                .remove(&id);
            if let Some(request) = request {
                let _ = request.sender.send(FrontInvokeResponse {
                    id,
                    success: false,
                    data: None,
                    error: Some("frontend readiness generation changed".to_string()),
                    readiness_generation: Some(generation),
                });
            }
        }
    }

    fn add_pending(
        &self,
        id: u64,
        sender: oneshot::Sender<FrontInvokeResponse>,
        readiness_generation: Option<u64>,
    ) {
        self.pending
            .lock()
            .expect("frontbridge pending map poisoned")
            .insert(
                id,
                PendingRequest {
                    sender,
                    readiness_generation,
                },
            );
    }

    fn remove_pending(&self, id: u64) {
        self.pending
            .lock()
            .expect("frontbridge pending map poisoned")
            .remove(&id);
    }
}

static FRONT_INVOKE_STATE: OnceCell<Arc<FrontInvokeState>> = OnceCell::new();

#[cfg(feature = "test-hook")]
type FrontInvokeTestHandler =
    Arc<dyn Fn(&str, Option<&Value>) -> Option<Result<Value>> + Send + Sync + 'static>;

#[cfg(feature = "test-hook")]
static FRONT_INVOKE_TEST_HANDLER: OnceCell<FrontInvokeTestHandler> = OnceCell::new();

#[cfg(feature = "test-hook")]
pub fn set_test_handler(
    handler: impl Fn(&str, Option<&Value>) -> Option<Result<Value>> + Send + Sync + 'static,
) {
    let _ = FRONT_INVOKE_TEST_HANDLER.set(Arc::new(handler));
}

fn state<R: Runtime>(app_handle: &AppHandle<R>) -> Arc<FrontInvokeState> {
    Arc::clone(FRONT_INVOKE_STATE.get_or_init(|| {
        let state = Arc::new(FrontInvokeState::new());
        state.register_listener(app_handle);
        state
    }))
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub async fn invoke_frontend<T, P>(
    app_handle: &AppHandle<impl Runtime>,
    method: impl Into<String>,
    payload: P,
) -> Result<T>
where
    T: DeserializeOwned,
    P: Serialize,
{
    invoke_frontend_with_options(app_handle, method, payload, InvokeOptions::default()).await
}

pub async fn invoke_frontend_with_options<T, P>(
    app_handle: &AppHandle<impl Runtime>,
    method: impl Into<String>,
    payload: P,
    options: InvokeOptions,
) -> Result<T>
where
    T: DeserializeOwned,
    P: Serialize,
{
    let method = method.into();
    #[cfg(feature = "test-hook")]
    if let Some(handler) = FRONT_INVOKE_TEST_HANDLER.get() {
        let payload_value = serde_json::to_value(&payload).context("serialize frontend payload")?;
        if let Some(response) = handler(
            &method,
            (!payload_value.is_null()).then_some(&payload_value),
        ) {
            return response.and_then(|value| {
                serde_json::from_value(value).context("deserialize frontend test response")
            });
        }
    }

    let deadline_at_ms = options
        .deadline_at_ms
        .or_else(|| options.timeout.map(|t| unix_now_ms().saturating_add(t.as_millis() as u64)));
    if let Some(deadline) = deadline_at_ms {
        let now = unix_now_ms();
        if deadline <= now {
            return Err(anyhow!("frontend invoke {method} deadline expired"));
        }
    }

    let state = state(app_handle);
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let _pending_guard = PendingRequestGuard {
        state: Arc::clone(&state),
        id,
    };
    let request_id = options
        .request_id
        .filter(|request_id| !request_id.trim().is_empty())
        .unwrap_or_else(|| format!("front-{id}"));
    let (tx, rx) = oneshot::channel();
    state.add_pending(id, tx, options.readiness_generation);

    let payload_value = match serde_json::to_value(payload).context("serialize frontend payload") {
        Ok(value) => value,
        Err(error) => {
            state.remove_pending(id);
            return Err(error);
        }
    };
    let request = FrontInvokeRequest {
        id,
        method: method.clone(),
        client_id: options.client_id,
        request_id,
        deadline_at_ms,
        readiness_generation: options.readiness_generation,
        payload: (!payload_value.is_null()).then_some(payload_value),
    };

    let emit_result = if let Some(window) = app_handle.get_webview_window("main") {
        window
            .emit(REQUEST_EVENT, &request)
            .context("emit frontend invoke event (main)")
    } else {
        app_handle
            .emit(REQUEST_EVENT, &request)
            .context("emit frontend invoke event")
    };
    if let Err(error) = emit_result {
        return Err(error);
    }

    let resp = match options.timeout {
        Some(timeout) => {
            let remaining = match deadline_at_ms {
                Some(deadline) => {
                    Duration::from_millis(deadline.saturating_sub(unix_now_ms())).min(timeout)
                }
                None => timeout,
            };
            match tokio::time::timeout(remaining, rx).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(_)) => {
                    return Err(anyhow!("frontend invoke {method} dropped without response"));
                }
                Err(_) => {
                    return Err(anyhow!("frontend invoke {method} timed out"));
                }
            }
        }
        None => match rx.await {
            Ok(resp) => resp,
            Err(_) => {
                return Err(anyhow!("frontend invoke {method} dropped without response"));
            }
        },
    };

    if let Some(expected_generation) = options.readiness_generation {
        if let Some(actual_generation) = resp.readiness_generation {
            if actual_generation != expected_generation {
                return Err(anyhow!(
                    "frontend invoke {method} returned obsolete generation {actual_generation} (expected {expected_generation})"
                ));
            }
        }
    }

    if resp.success {
        let value = resp.data.unwrap_or(Value::Null);
        serde_json::from_value(value).context("deserialize frontend response")
    } else {
        Err(anyhow!(
            "frontend invoke {method} failed: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_have_a_bounded_timeout() {
        assert_eq!(InvokeOptions::default().timeout, Some(DEFAULT_TIMEOUT));
    }

    #[test]
    fn infinite_options_have_no_timeout_or_deadline() {
        assert_eq!(InvokeOptions::infinite().timeout, None);
        assert_eq!(InvokeOptions::infinite().deadline_at_ms, None);
    }

    #[test]
    fn unix_clock_is_non_zero() {
        assert!(unix_now_ms() > 0);
    }
}
