use std::{io::Cursor, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION},
    Url,
};
use serde_json::Value;

use crate::{
    model::{append_standard_endpoint, is_sensitive_header, ModelType},
    store::{
        ImageGenerationKeyAction, ImageGenerationMode, ImageGenerationSettings,
        ImageGenerationSettingsInput, ImageGenerationSettingsView, Store,
    },
    Error, Result,
};

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 16 * 1024;

pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

struct ImageConnection {
    url: Url,
    api_key: String,
    custom_headers: Value,
}

impl ImageConnection {
    fn headers(&self, conversation_id: &str) -> Result<HeaderMap> {
        let mut headers = crate::provider::custom_headers(&self.custom_headers, conversation_id)?;
        if !self.api_key.is_empty() && !headers.contains_key(AUTHORIZATION) {
            let value = HeaderValue::from_str(&format!("Bearer {}", self.api_key))
                .map_err(|_| Error::Config("invalid image API key".into()))?;
            headers.insert(AUTHORIZATION, value);
        }
        for (name, value) in &mut headers {
            if is_sensitive_header(name.as_str()) {
                value.set_sensitive(true);
            }
        }
        Ok(headers)
    }

    fn models_url(&self) -> Result<Url> {
        let mut url = self.url.clone();
        let path = url.path().trim_end_matches('/');
        let prefix = path.strip_suffix("/images/generations").ok_or_else(|| {
            Error::Config(
                "cannot derive a model catalog from this image URL; enter the image model manually"
                    .into(),
            )
        })?;
        url.set_path(&format!("{prefix}/models"));
        Ok(url)
    }
}

pub async fn settings_view(
    store: &Store,
    settings: ImageGenerationSettings,
) -> Result<ImageGenerationSettingsView> {
    let has_selection = match settings.mode {
        ImageGenerationMode::Existing => !settings.connection_model_hash.is_empty(),
        ImageGenerationMode::Independent => !settings.base_url.is_empty(),
    };
    let (available, endpoint_url, configuration_error) = if settings.enabled || has_selection {
        match resolve_connection(store, &settings).await {
            Ok(connection) => {
                let mut url = connection.url;
                url.set_query(None);
                (true, Some(url.to_string()), None)
            }
            Err(Error::Config(message)) => (false, None, Some(message)),
            Err(error) => return Err(error),
        }
    } else {
        (false, None, None)
    };
    Ok(ImageGenerationSettingsView {
        enabled: settings.enabled,
        mode: settings.mode,
        connection_model_hash: settings.connection_model_hash,
        base_url: settings.base_url,
        use_full_url: settings.use_full_url,
        image_model: settings.image_model,
        has_api_key: !settings.api_key.is_empty(),
        configured: settings.enabled && available,
        configuration_error,
        endpoint_url,
    })
}

pub async fn prepare_settings(
    store: &Store,
    input: ImageGenerationSettingsInput,
) -> Result<ImageGenerationSettings> {
    let current = store.image_generation_settings().await?;
    if input.mode == ImageGenerationMode::Existing {
        let settings = ImageGenerationSettings {
            enabled: input.enabled,
            mode: ImageGenerationMode::Existing,
            connection_model_hash: input.connection_model_hash.trim().to_owned(),
            image_model: input.image_model.trim().to_owned(),
            ..current
        };
        if settings.enabled {
            resolve_connection(store, &settings).await?;
        }
        return Ok(settings);
    }
    let base_url = input.base_url.trim().to_owned();
    if !base_url.is_empty() {
        validate_url(&base_url)?;
    }
    let supplied_key = input.api_key.as_deref().unwrap_or("").trim();
    let api_key = match input.key_action {
        ImageGenerationKeyAction::Keep => {
            if !supplied_key.is_empty() {
                return Err(Error::Config(
                    "select replace to update the image API key".into(),
                ));
            }
            if !current.api_key.is_empty() && origin(&base_url)? != origin(&current.base_url)? {
                return Err(Error::Config(
                    "the image API origin changed; replace or explicitly clear the saved API key"
                        .into(),
                ));
            }
            current.api_key
        }
        ImageGenerationKeyAction::Replace => {
            if supplied_key.is_empty() {
                return Err(Error::Config(
                    "a replacement image API key is required".into(),
                ));
            }
            supplied_key.to_owned()
        }
        ImageGenerationKeyAction::Clear => {
            if !supplied_key.is_empty() {
                return Err(Error::Config(
                    "cannot replace and clear the image API key together".into(),
                ));
            }
            String::new()
        }
    };
    let settings = ImageGenerationSettings {
        enabled: input.enabled,
        mode: input.mode,
        connection_model_hash: input.connection_model_hash.trim().to_owned(),
        base_url,
        use_full_url: input.use_full_url,
        api_key,
        image_model: input.image_model.trim().to_owned(),
    };
    if settings.enabled {
        resolve_connection(store, &settings).await?;
    }
    Ok(settings)
}

pub async fn is_available(store: &Store) -> Result<bool> {
    let settings = store.image_generation_settings().await?;
    if !settings.is_configured() {
        return Ok(false);
    }
    match resolve_connection(store, &settings).await {
        Ok(_) => Ok(true),
        Err(Error::Config(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

pub async fn model_catalog(store: &Store, input: ImageGenerationSettingsInput) -> Result<Value> {
    let settings = prepare_settings(store, input).await?;
    let connection = resolve_connection(store, &settings).await?;
    let headers = connection.headers("")?;
    let response = image_client(store, Duration::from_secs(30))
        .await?
        .get(connection.models_url()?)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|error| Error::Http(error.without_url()))?;
    let body = response_body(response, MAX_CATALOG_BYTES, "Image model catalog", &headers).await?;
    let value: Value = serde_json::from_slice(&body)?;
    if !value.get("data").unwrap_or(&value).is_array() {
        return Err(Error::provider(
            "image model catalog does not contain a model list".into(),
        ));
    }
    Ok(value)
}

pub async fn generate(
    store: &Store,
    conversation_id: &str,
    description: &str,
    aspect_ratio: Option<&str>,
) -> Result<GeneratedImage> {
    let settings = store.image_generation_settings().await?;
    if !settings.is_configured() {
        return Err(Error::Config(
            "image generation is disabled or not configured".into(),
        ));
    }
    let connection = resolve_connection(store, &settings).await?;
    let headers = connection.headers(conversation_id)?;
    let size = aspect_ratio.map(image_size).transpose()?;
    let mut body = serde_json::json!({
        "prompt": description,
        "n": 1,
        "response_format": "b64_json",
    });
    if !settings.image_model.is_empty() {
        body["model"] = Value::String(settings.image_model);
    }
    if let Some(size) = size {
        body["size"] = Value::String(size.into());
    }
    tracing::info!("sending independent image generation request");
    let response = image_client(store, Duration::from_secs(180))
        .await?
        .post(connection.url)
        .headers(headers.clone())
        .json(&body)
        .send()
        .await
        .map_err(|error| Error::Http(error.without_url()))?;
    let bytes = response_body(response, MAX_RESPONSE_BYTES, "OpenAI Images", &headers).await?;
    let payload: Value = serde_json::from_slice(&bytes)?;
    let encoded = payload
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::provider(
                "image response does not contain b64_json; no image was generated for display"
                    .into(),
            )
        })?;
    if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(Error::provider(
            "generated image exceeds the size limit".into(),
        ));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| Error::provider("generated image is not valid base64".into()))?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(Error::provider(
            "generated image exceeds the size limit".into(),
        ));
    }
    let format = image::guess_format(&bytes)
        .map_err(|_| Error::provider("generated image format is invalid".into()))?;
    let mime_type = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::WebP => "image/webp",
        _ => {
            return Err(Error::provider(
                "generated image must be PNG, JPEG, or WebP".into(),
            ))
        }
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|_| {
        Error::provider("generated image exceeds decoder limits or cannot be decoded".into())
    })?;
    Ok(GeneratedImage {
        bytes,
        mime_type: mime_type.into(),
    })
}

async fn resolve_connection(
    store: &Store,
    settings: &ImageGenerationSettings,
) -> Result<ImageConnection> {
    let connection = match settings.mode {
        ImageGenerationMode::Independent => {
            if settings.base_url.is_empty() {
                return Err(Error::Config(
                    "an independent image API address is required".into(),
                ));
            }
            let base = validate_url(&settings.base_url)?;
            let url = if settings.use_full_url {
                base
            } else {
                Url::parse(&append_standard_endpoint(
                    base.as_str(),
                    "/v1/images/generations",
                )?)
                .map_err(|_| Error::Config("invalid image API endpoint".into()))?
            };
            ImageConnection {
                url,
                api_key: settings.api_key.clone(),
                custom_headers: serde_json::json!({}),
            }
        }
        ImageGenerationMode::Existing => {
            if settings.connection_model_hash.is_empty() {
                return Err(Error::Config(
                    "select an existing image API connection".into(),
                ));
            }
            let model = store
                .model(&settings.connection_model_hash)
                .await?
                .ok_or_else(|| {
                    Error::Config(
                        "the selected image API connection no longer exists; select it again"
                            .into(),
                    )
                })?;
            if model.model_type != ModelType::OpenAi {
                return Err(Error::Config(
                    "the image API connection must be OpenAI-compatible".into(),
                ));
            }
            let mut url = validate_url(&model.base_url)?;
            if model.use_full_url {
                let path = url.path().trim_end_matches('/');
                let path = if path.ends_with("/images/generations") {
                    path.to_owned()
                } else if let Some(prefix) = path
                    .strip_suffix("/responses")
                    .or_else(|| path.strip_suffix("/chat/completions"))
                {
                    format!("{prefix}/images/generations")
                } else {
                    return Err(Error::Config("cannot derive an image endpoint from this complete chat URL; configure an independent complete image URL".into()));
                };
                url.set_path(&path);
            } else {
                url = Url::parse(&append_standard_endpoint(
                    url.as_str(),
                    "/v1/images/generations",
                )?)
                .map_err(|_| Error::Config("invalid image API endpoint".into()))?;
            }
            ImageConnection {
                url,
                api_key: model.api_key,
                custom_headers: if model.custom_headers_enabled {
                    model.custom_headers
                } else {
                    serde_json::json!({})
                },
            }
        }
    };
    connection.headers("")?;
    Ok(connection)
}

fn validate_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| Error::Config("invalid image API address".into()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::Config(
            "the image API address must be an HTTP(S) URL with a host".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::Config(
            "the image API address cannot contain credentials or a fragment".into(),
        ));
    }
    if url.query_pairs().any(|(name, _)| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "api_key" | "api-key" | "key" | "token" | "access_token" | "authorization" | "password"
        )
    }) {
        return Err(Error::Config(
            "put image API credentials in the API key field, not the URL".into(),
        ));
    }
    Ok(url)
}

fn origin(value: &str) -> Result<Option<url::Origin>> {
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(validate_url(value)?.origin()))
    }
}

async fn image_client(store: &Store, timeout: Duration) -> Result<reqwest::Client> {
    Ok(crate::network::client_builder(store)
        .await?
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

async fn response_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
    headers: &HeaderMap,
) -> Result<Vec<u8>> {
    let status = response.status();
    let limit = if status.is_success() {
        max_bytes
    } else {
        MAX_ERROR_BYTES
    };
    if status.is_success()
        && response
            .content_length()
            .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::provider(format!(
            "{label} response exceeds the size limit"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| Error::Http(error.without_url()))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            if status.is_success() {
                return Err(Error::provider(format!(
                    "{label} response exceeds the size limit"
                )));
            }
            body.extend_from_slice(&chunk[..limit.saturating_sub(body.len())]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let mut message = if status.as_u16() == 404 {
            "endpoint not found; check the image API URL in settings. No image was generated."
                .to_owned()
        } else if status.is_redirection() {
            "redirects are not followed; configure the final API URL".to_owned()
        } else {
            String::from_utf8_lossy(&body).into_owned()
        };
        let mut parsed = serde_json::from_str::<Value>(&message).ok();
        for (name, value) in headers {
            if is_sensitive_header(name.as_str()) {
                if let Ok(secret) = std::str::from_utf8(value.as_bytes()) {
                    for secret in [secret, secret.strip_prefix("Bearer ").unwrap_or(secret)] {
                        if !secret.is_empty() {
                            message = message.replace(secret, "[REDACTED]");
                            if let Some(value) = &mut parsed {
                                redact_error_value(value, secret);
                            }
                        }
                    }
                }
            }
        }
        if let Some(value) = parsed {
            message = serde_json::to_string(&value)?;
        }
        return Err(Error::Provider(crate::ProviderFailure::http(
            label,
            status.as_u16(),
            &message,
        )));
    }
    Ok(body)
}

fn redact_error_value(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => *text = text.replace(secret, "[REDACTED]"),
        Value::Array(values) => {
            for value in values {
                redact_error_value(value, secret);
            }
        }
        Value::Object(fields) => {
            for value in fields.values_mut() {
                redact_error_value(value, secret);
            }
        }
        _ => {}
    }
}

fn image_size(aspect_ratio: &str) -> Result<&'static str> {
    match aspect_ratio {
        "1:1" => Ok("1024x1024"),
        "4:3" | "16:9" => Ok("1536x1024"),
        "3:4" | "9:16" => Ok("1024x1536"),
        _ => Err(Error::Protocol(format!(
            "unsupported GenerateImage aspect_ratio: {aspect_ratio}"
        ))),
    }
}
