//! Native wire and Protobuf JSON codecs generated at project build time.
use crate::error::AppError;
use prost::Message;
use serde::{de::DeserializeOwned, Serialize};
// Each protocol family has independent generated messages and dispatch tables.
#[allow(clippy::all, dead_code)]
mod jp {
    use super::*;
    include!(concat!(env!("OUT_DIR"), "/jp/messages.rs"));
    include!(concat!(env!("OUT_DIR"), "/jp/dispatch.rs"));
}
#[allow(clippy::all, dead_code)]
mod global {
    use super::*;
    include!(concat!(env!("OUT_DIR"), "/global/messages.rs"));
    include!(concat!(env!("OUT_DIR"), "/global/dispatch.rs"));
}
#[cfg(test)]
pub const SHA256: &str = jp::SHA256;
pub fn fingerprint(family: &str) -> &'static str {
    if family == "global" {
        global::SHA256
    } else {
        jp::SHA256
    }
}
pub fn encode(
    family: &str,
    route: &str,
    value: &serde_json::Value,
) -> Result<Option<Vec<u8>>, AppError> {
    if family == "global" {
        global::encode(route, value)
    } else {
        jp::encode(route, value)
    }
}
pub fn decode(
    family: &str,
    route: &str,
    bytes: &[u8],
) -> Result<Option<serde_json::Value>, AppError> {
    if family == "global" {
        global::decode(route, bytes)
    } else {
        jp::decode(route, bytes)
    }
}
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
