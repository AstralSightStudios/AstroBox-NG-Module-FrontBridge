use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Listener};
use tokio::sync::oneshot;

pub const REQUEST_EVENT: &str = "astrobox://frontinvoke/request";
pub const RESPONSE_EVENT: &str = "astrobox://frontinvoke/response";

#[derive(Debug, Serialize)]
struct FrontInvokeRequest {
    id: u64,
    method: String,
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
}

struct FrontInvokeState {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<FrontInvokeResponse>>>,
}

impl FrontInvokeState {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn register_listener(self: &Arc<Self>, app_handle: &AppHandle) {
        let state = Arc::clone(self);
        let _ = app_handle.listen_any(RESPONSE_EVENT, move |event| {
            let payload = event.payload();
            match serde_json::from_str::<FrontInvokeResponse>(payload) {
                Ok(resp) => state.resolve(resp),
                Err(err) => {
                    log::error!("[frontbridge] failed to parse response payload: {err}");
                }
            }
        });
    }

    fn resolve(&self, resp: FrontInvokeResponse) {
        let sender = self
            .pending
            .lock()
            .expect("frontbridge pending map poisoned")
            .remove(&resp.id);
        if let Some(tx) = sender {
            let _ = tx.send(resp);
        } else {
            log::warn!(
                "[frontbridge] no pending request for response id={}",
                resp.id
            );
        }
    }

    fn add_pending(&self, id: u64, sender: oneshot::Sender<FrontInvokeResponse>) {
        self.pending
            .lock()
            .expect("frontbridge pending map poisoned")
            .insert(id, sender);
    }
}

static FRONT_INVOKE_STATE: OnceCell<Arc<FrontInvokeState>> = OnceCell::new();

fn state(app_handle: &AppHandle) -> Arc<FrontInvokeState> {
    Arc::clone(FRONT_INVOKE_STATE.get_or_init(|| {
        let state = Arc::new(FrontInvokeState::new());
        state.register_listener(app_handle);
        state
    }))
}

pub async fn invoke_frontend<R, P>(
    app_handle: &AppHandle,
    method: impl Into<String>,
    payload: P,
) -> Result<R>
where
    R: DeserializeOwned,
    P: Serialize,
{
    let method = method.into();
    let state = state(app_handle);
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    state.add_pending(id, tx);

    let payload_value = serde_json::to_value(payload).context("serialize frontend payload")?;
    let request = FrontInvokeRequest {
        id,
        method: method.clone(),
        payload: (!payload_value.is_null()).then_some(payload_value),
    };

    app_handle
        .emit(REQUEST_EVENT, &request)
        .context("emit frontend invoke event")?;

    let resp = rx
        .await
        .map_err(|_| anyhow!("frontend invoke {method} dropped without response"))?;

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
