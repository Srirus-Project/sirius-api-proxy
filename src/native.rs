//! Native wire and Protobuf JSON codecs generated at project build time.
use crate::error::AppError;
use prost::Message;
use serde::{de::DeserializeOwned, Serialize};
// Lints in third-party generated code are not actionable in this module.
#[allow(clippy::all, dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}
use generated::*;
include!(concat!(env!("OUT_DIR"), "/dispatch.rs"));
fn encode_message<T: Message + DeserializeOwned>(
    value: &serde_json::Value,
) -> Result<Option<Vec<u8>>, AppError> {
    // pbjson rejects unknown numeric enums; the caller uses the pinned dynamic
    // schema for JSON cases unsupported by the generator.
    Ok(T::deserialize(value)
        .ok()
        .map(|message| message.encode_to_vec()))
}
fn decode_message<T: Message + Default + Serialize>(
    bytes: &[u8],
) -> Result<Option<serde_json::Value>, AppError> {
    let message = T::decode(bytes).map_err(|_| AppError::Protocol)?;
    Ok(serde_json::to_value(message).ok())
}
