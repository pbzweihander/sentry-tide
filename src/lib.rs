use std::borrow::Cow;
use std::sync::Arc;

use sentry_anyhow::AnyhowHubExt;
use sentry_core::protocol::{ClientSdkPackage, Event, Request as SentryRequest};
use sentry_core::{Hub, SentryFutureExt};
use tide::Request;

#[derive(Debug)]
pub struct SentryMiddleware {
    hub: Option<Arc<Hub>>,
    emit_header: bool,
    capture_server_errors: bool,
}

impl SentryMiddleware {
    pub fn new() -> Self {
        Self {
            hub: None,
            emit_header: false,
            capture_server_errors: true,
        }
    }

    /// Reconfigures the middleware so that it uses a specific hub instead of the default one.
    pub fn with_hub(mut self, hub: Arc<Hub>) -> Self {
        self.hub = Some(hub);
        self
    }

    /// Reconfigures the middleware so that it uses a specific hub instead of the default one.
    pub fn with_default_hub(mut self) -> Self {
        self.hub = None;
        self
    }

    /// If configured the sentry id is attached to a X-Sentry-Event header.
    pub fn emit_header(mut self, val: bool) -> Self {
        self.emit_header = val;
        self
    }

    /// Enables or disables error reporting.
    ///
    /// The default is to report all errors.
    pub fn capture_server_errors(mut self, val: bool) -> Self {
        self.capture_server_errors = val;
        self
    }
}

impl Default for SentryMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl<State> tide::Middleware<State> for SentryMiddleware
where
    State: Clone + Send + Sync + 'static,
{
    async fn handle(&self, request: Request<State>, next: tide::Next<'_, State>) -> tide::Result {
        let hub = Arc::new(Hub::new_from_top(Hub::main()));
        let client = hub.client();
        let with_pii = client
            .as_ref()
            .map_or(false, |x| x.options().send_default_pii);

        let (tx, sentry_req) = sentry_request_from_http(&request, with_pii);
        hub.configure_scope(|scope| {
            scope.set_transaction(tx.as_deref());
            scope.add_event_processor(Box::new(move |event| {
                Some(process_event(event, &sentry_req))
            }));
        });

        let mut response = next.run(request).bind_hub(hub.clone()).await;
        if self.capture_server_errors && response.status().is_server_error() {
            if let Some(error) = response.take_error() {
                let status = error.status();
                let anyhow_error = error.into_inner();
                let event_id = hub.capture_anyhow(&anyhow_error);

                if self.emit_header {
                    response.insert_header("x-sentry-event", event_id.to_simple_ref().to_string());
                }
                response.set_error(tide::Error::new(status, anyhow_error));
            }
        }

        Ok(response)
    }
}

/// Build a Sentry request struct from the HTTP request
fn sentry_request_from_http<State>(
    request: &Request<State>,
    with_pii: bool,
) -> (Option<String>, SentryRequest) {
    // TODO: better route information
    let transaction = Some(request.url().path().to_string());

    let mut sentry_req = SentryRequest {
        url: Some(request.url().clone()),
        method: Some(request.method().to_string()),
        headers: request
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };

    // If PII is enabled, include the remote address
    if with_pii {
        if let Some(remote) = request.remote() {
            sentry_req.env.insert("REMOTE_ADDR".into(), remote.into());
        }
    };

    (transaction, sentry_req)
}

/// Add request data to a Sentry event
fn process_event(mut event: Event<'static>, request: &SentryRequest) -> Event<'static> {
    // Request
    if event.request.is_none() {
        event.request = Some(request.clone());
    }

    // SDK
    if let Some(sdk) = event.sdk.take() {
        let mut sdk = sdk.into_owned();
        sdk.packages.push(ClientSdkPackage {
            name: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
        });
        event.sdk = Some(Cow::Owned(sdk));
    }
    event
}
