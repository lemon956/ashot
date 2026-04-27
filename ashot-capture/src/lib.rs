use std::collections::HashMap;

use ashot_ipc::CaptureMode;
use futures_util::StreamExt;
use thiserror::Error;
use url::Url;
use uuid::Uuid;
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue, SerializeDict, Type},
};

const DESKTOP_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENSHOT_INTERFACE: &str = "org.freedesktop.portal.Screenshot";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("the screenshot portal is unavailable: {0}")]
    Portal(#[from] zbus::Error),
    #[error("the portal request returned no response")]
    NoResponse,
    #[error("the screenshot request was cancelled")]
    Cancelled,
    #[error("the screenshot portal finished with an unspecified error")]
    Other,
    #[error("the screenshot portal returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("the portal returned a non-file URI: {0}")]
    NonFileUri(String),
}

#[derive(Debug, Clone)]
pub struct CaptureClient {
    connection: Connection,
}

impl Default for CaptureClient {
    fn default() -> Self {
        panic!("CaptureClient::default is not supported; use CaptureClient::new instead")
    }
}

impl CaptureClient {
    pub async fn new() -> Result<Self, CaptureError> {
        Ok(Self { connection: Connection::session().await? })
    }

    pub async fn capture(
        &self,
        mode: CaptureMode,
        parent_window: Option<&str>,
    ) -> Result<Url, CaptureError> {
        let screenshot =
            Proxy::new(&self.connection, DESKTOP_DESTINATION, DESKTOP_PATH, SCREENSHOT_INTERFACE)
                .await?;
        let options = ScreenshotOptions::new(mode);
        let request_path: OwnedObjectPath =
            screenshot.call("Screenshot", &(parent_window.unwrap_or(""), options)).await?;

        let request = Proxy::new(
            &self.connection,
            DESKTOP_DESTINATION,
            request_path.as_ref(),
            REQUEST_INTERFACE,
        )
        .await?;
        let mut responses = request.receive_signal("Response").await?;
        let message = responses.next().await.ok_or(CaptureError::NoResponse)?;
        let (status, results): (u32, HashMap<String, OwnedValue>) = message
            .body()
            .deserialize()
            .map_err(|error| CaptureError::InvalidResponse(error.to_string()))?;

        match status {
            0 => {
                let uri_value = results
                    .get("uri")
                    .ok_or_else(|| CaptureError::InvalidResponse("missing `uri` result".into()))?;
                let uri_string = String::try_from(uri_value.clone()).map_err(|error| {
                    CaptureError::InvalidResponse(format!("invalid `uri` value: {error}"))
                })?;
                let uri = Url::parse(&uri_string)
                    .map_err(|error| CaptureError::InvalidResponse(error.to_string()))?;
                if uri.scheme() != "file" {
                    return Err(CaptureError::NonFileUri(uri.to_string()));
                }
                Ok(uri)
            }
            1 => Err(CaptureError::Cancelled),
            2 => Err(CaptureError::Other),
            other => Err(CaptureError::InvalidResponse(format!(
                "unexpected portal response status `{other}`"
            ))),
        }
    }
}

#[derive(Debug, SerializeDict, Type)]
#[zvariant(signature = "dict")]
struct ScreenshotOptions {
    handle_token: String,
    modal: Option<bool>,
    interactive: Option<bool>,
}

impl ScreenshotOptions {
    fn new(mode: CaptureMode) -> Self {
        Self {
            handle_token: format!("ashot{}", Uuid::new_v4().simple()),
            modal: Some(true),
            interactive: Some(!matches!(mode, CaptureMode::Screen)),
        }
    }
}

#[cfg(test)]
mod tests {
    use ashot_ipc::CaptureMode;

    use super::ScreenshotOptions;

    #[test]
    fn portal_picker_is_interactive_for_area_and_window_capture() {
        assert_eq!(ScreenshotOptions::new(CaptureMode::Area).interactive, Some(true));
        assert_eq!(ScreenshotOptions::new(CaptureMode::Window).interactive, Some(true));
    }

    #[test]
    fn portal_picker_is_not_interactive_for_fullscreen_capture() {
        assert_eq!(ScreenshotOptions::new(CaptureMode::Screen).interactive, Some(false));
    }
}
