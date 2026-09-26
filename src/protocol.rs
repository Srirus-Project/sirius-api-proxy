//! Runtime .proto bundles. Compilation and imports use a bounded in-memory snapshot.
use crate::{
    client::{self, *},
    error::AppError,
};
use chrono::{DateTime, Utc};
use prost::Message;
use prost_reflect::{Cardinality, DescriptorPool, Kind, MessageDescriptor, MethodDescriptor};
use serde::Serialize;
use std::{collections::HashSet, path::Path};

pub use crate::routes::ROUTES;
#[derive(Clone, Serialize)]
pub struct ProtocolStatus {
    pub family: String,
    pub version: String,
    pub sha256: String,
    pub generation: u64,
    pub loaded_at: DateTime<Utc>,
    pub files: usize,
    pub source: &'static str,
    pub codec: &'static str,
    pub native_sha256: &'static str,
}
pub struct ProtocolBundle {
    pub pool: DescriptorPool,
    pub status: ProtocolStatus,
}
pub fn method(pool: &DescriptorPool, route: &str) -> Result<MethodDescriptor, AppError> {
    let (service, name) = route
        .trim_start_matches('/')
        .rsplit_once('/')
        .ok_or(AppError::ProtocolDefinition)?;
    pool.get_service_by_name(service)
        .and_then(|s| s.methods().find(|m| m.name() == name))
        .ok_or(AppError::ProtocolDefinition)
}
impl ProtocolBundle {
    pub(crate) fn encode(
        &self,
        route: &str,
        value: serde_json::Value,
    ) -> Result<Vec<u8>, AppError> {
        if self.status.codec == "native" {
            if let Some(encoded) = crate::native::encode(&self.status.family, route, &value)? {
                return Ok(encoded);
            }
        }
        let method = method(&self.pool, route)?;
        let message = prost_reflect::DynamicMessage::deserialize(method.input(), value)
            .map_err(|_| AppError::InvalidRequest)?;
        Ok(message.encode_to_vec())
    }
    pub(crate) fn decode(&self, route: &str, bytes: &[u8]) -> Result<serde_json::Value, AppError> {
        if self.status.codec == "native" {
            if let Some(decoded) = crate::native::decode(&self.status.family, route, bytes)? {
                return Ok(decoded);
            }
        }
        let method = method(&self.pool, route)?;
        let message = prost_reflect::DynamicMessage::decode(method.output(), bytes)
            .map_err(|_| AppError::Protocol)?;
        serde_json::to_value(message).map_err(|_| AppError::Protocol)
    }

    pub fn load(directory: &Path) -> Result<Self, AppError> {
        let compiled =
            crate::proto_source::compile(directory).map_err(|_| AppError::ProtocolDefinition)?;
        let pool = DescriptorPool::decode(compiled.encoded.as_slice())
            .map_err(|_| AppError::ProtocolDefinition)?;
        validate_contract(&pool, &compiled.family)?;
        let native_sha256 = crate::native::fingerprint(&compiled.family);
        let codec = if compiled.sha256 == native_sha256 {
            "native"
        } else {
            "dynamic"
        };
        Ok(Self {
            pool,
            status: ProtocolStatus {
                family: compiled.family,
                version: compiled.version,
                sha256: compiled.sha256,
                generation: 1,
                loaded_at: Utc::now(),
                files: compiled.files,
                source: "proto",
                codec,
                native_sha256,
            },
        })
    }
}
fn invalid<T>() -> Result<T, AppError> {
    Err(AppError::ProtocolDefinition)
}
fn field(
    message: &MessageDescriptor,
    name: &str,
    kind: Kind,
    repeated: bool,
) -> Result<(), AppError> {
    let f = message
        .fields()
        .find(|f| f.json_name() == name)
        .ok_or(AppError::ProtocolDefinition)?;
    if f.kind() != kind || f.is_list() != repeated || f.is_map() {
        return invalid();
    }
    Ok(())
}
/// Field `number` must exist with the given JSON name (when given), kind and cardinality.
fn numbered(
    message: &MessageDescriptor,
    number: u32,
    json: Option<&str>,
    kind: Kind,
    repeated: bool,
) -> Result<(), AppError> {
    let f = message
        .get_field(number)
        .ok_or(AppError::ProtocolDefinition)?;
    if json.is_some_and(|name| f.json_name() != name)
        || f.kind() != kind
        || f.is_list() != repeated
        || f.is_map()
    {
        return invalid();
    }
    Ok(())
}
/// JSON name of `ServerInfo` field 8 (the area ID). The Global client names it `areaID`; the
/// proxy publishes it as `areaId` regardless (see [`normalize_servers`]).
pub(crate) fn server_area_json_name(pool: &DescriptorPool) -> Option<String> {
    let m = method(pool, crate::routes::SERVER_LIST).ok()?;
    let Kind::Message(info) = m.output().get_field(1)?.kind() else {
        return None;
    };
    Some(info.get_field(8)?.json_name().to_owned())
}
/// Keep the public `/api/v1/servers` key `areaId` whatever the bundle names field 8.
pub(crate) fn normalize_servers(pool: &DescriptorPool, value: &mut serde_json::Value) {
    let Some(name) = server_area_json_name(pool).filter(|n| n != "areaId") else {
        return;
    };
    if let Some(servers) = value.get_mut("servers").and_then(|v| v.as_array_mut()) {
        for server in servers.iter_mut().filter_map(|s| s.as_object_mut()) {
            if let Some(area) = server.remove(&name) {
                server.insert("areaId".into(), area);
            }
        }
    }
}
fn validate_contract(pool: &DescriptorPool, family: &str) -> Result<(), AppError> {
    let skip_auth = pool
        .get_extension_by_name("entity.method_options.skip_authentication")
        .ok_or(AppError::ProtocolDefinition)?;
    for route in crate::routes::contract_for_family(family) {
        let m = method(pool, route)?;
        if m.is_client_streaming() || m.is_server_streaming() {
            return invalid();
        }
        if m.options().get_extension(&skip_auth).as_bool() != Some(!client::authenticated(route)) {
            return invalid();
        }
        if m.input()
            .fields()
            .any(|f| f.cardinality() == Cardinality::Required)
        {
            return invalid();
        }
        match *route {
            ANNOUNCEMENT => field(&m.input(), "id", Kind::Int64, false)?,
            PROFILE => field(&m.input(), "playerProfileId", Kind::Int64, false)?,
            EVENT_RANKING => {
                field(&m.input(), "eventId", Kind::Int64, false)?;
                field(&m.input(), "ranks", Kind::Int32, true)?;
            }
            EVENT_DECK => {
                field(&m.input(), "eventId", Kind::Int64, false)?;
                field(&m.input(), "playerId", Kind::String, false)?;
            }
            MUSIC_RANKING => field(&m.input(), "musicId", Kind::Int64, false)?,
            CHALLENGE_RANKING => field(&m.input(), "challengeMusicId", Kind::Int64, false)?,
            VERSION => {
                field(&m.output(), "version", Kind::String, false)?;
                if family == "global" {
                    field(&m.output(), "resourceVersion", Kind::String, false)?;
                }
            }
            crate::routes::SERVER_LIST => {
                let server = m
                    .output()
                    .get_field(1)
                    .filter(|f| f.name() == "servers")
                    .ok_or(AppError::ProtocolDefinition)?;
                let Kind::Message(info) = server.kind() else {
                    return invalid();
                };
                if !server.is_list() || server.is_map() {
                    return invalid();
                }
                for (number, key) in [
                    (1, "name"),
                    (2, "cdnRoot"),
                    (3, "apiServerRoot"),
                    (4, "chatServerRoot"),
                    (5, "atServerRoot"),
                    (6, "liveServer"),
                    (7, "displayName"),
                    (9, "ageIconSpriteName"),
                ] {
                    numbered(&info, number, Some(key), Kind::String, false)?;
                }
                // The area ID is checked by number: the client's JSON name is `areaID`.
                numbered(&info, 8, None, Kind::String, false)?;
            }
            crate::routes::PLAYER_LOGIN => {
                let input = m.input();
                for (number, key) in [
                    (1, "sdkUid"),
                    (2, "sdkAccessToken"),
                    (4, "deviceModel"),
                    (5, "operatingSystem"),
                    (6, "clientVersion"),
                    (8, "clientPackage"),
                    (13, "idToken"),
                ] {
                    numbered(&input, number, Some(key), Kind::String, false)?;
                }
                for (number, key) in [
                    (3, "platform"),
                    (10, "globalChannelId"),
                    (11, "brandId"),
                    (12, "areaId"),
                ] {
                    numbered(&input, number, Some(key), Kind::Uint32, false)?;
                }
                let uuid = input.get_field(7).ok_or(AppError::ProtocolDefinition)?;
                let Kind::Message(uuid) = uuid.kind() else {
                    return invalid();
                };
                if input
                    .get_field(7)
                    .is_some_and(|f| f.json_name() != "uuid" || f.is_list())
                {
                    return invalid();
                }
                numbered(&uuid, 1, Some("adId"), Kind::String, false)?;
                numbered(&uuid, 2, Some("identifier"), Kind::String, false)?;
                let output = m.output();
                let credential = output.get_field(1).ok_or(AppError::ProtocolDefinition)?;
                let Kind::Message(credential_type) = credential.kind() else {
                    return invalid();
                };
                if credential.json_name() != "credential" || credential.is_list() {
                    return invalid();
                }
                numbered(&credential_type, 1, Some("id"), Kind::String, false)?;
                numbered(&credential_type, 2, Some("credential"), Kind::String, false)?;
                numbered(&output, 2, Some("cpServerId"), Kind::String, false)?;
                numbered(&output, 3, Some("cpServerName"), Kind::String, false)?;
                numbered(&output, 4, Some("isNewUser"), Kind::Uint32, false)?;
            }
            WHOAMI => field(&m.output(), "playerId", Kind::String, false)?,
            ANNOUNCEMENTS => {
                let f = m
                    .input()
                    .get_field_by_name("selected_tab")
                    .ok_or(AppError::ProtocolDefinition)?;
                let Kind::Enum(e) = f.kind() else {
                    return invalid();
                };
                if f.json_name() != "selectedTab"
                    || f.is_list()
                    || (0..=2).any(|n| e.get_value(n).is_none())
                {
                    return invalid();
                }
            }
            _ => {}
        }
    }
    Ok(())
}
/// Conservative additive compatibility for every type reachable from exposed RPCs.
/// Business routes and auth policy remain Rust code, never inferred from a new service.
pub(crate) fn compatible(old: &DescriptorPool, new: &DescriptorPool) -> Result<(), AppError> {
    let family = if method(old, crate::routes::SERVER_LIST).is_ok() {
        "global"
    } else {
        "jp"
    };
    let mut pending = Vec::new();
    let mut visited = HashSet::new();
    for route in crate::routes::contract_for_family(family) {
        let a = method(old, route)?;
        let b = method(new, route)?;
        if a.input().full_name() != b.input().full_name()
            || a.output().full_name() != b.output().full_name()
            || a.options().encode_to_vec() != b.options().encode_to_vec()
            || b.is_client_streaming()
            || b.is_server_streaming()
        {
            return invalid();
        }
        pending.extend([a.input(), a.output()]);
    }
    while let Some(a) = pending.pop() {
        if !visited.insert(a.full_name().to_owned()) {
            continue;
        }
        let b = new
            .get_message_by_name(a.full_name())
            .ok_or(AppError::ProtocolDefinition)?;
        if a.is_map_entry() != b.is_map_entry() {
            return invalid();
        }
        for added in b.fields() {
            if added.cardinality() == Cardinality::Required && a.get_field(added.number()).is_none()
            {
                return invalid();
            }
        }
        for x in a.fields() {
            let y = b
                .get_field(x.number())
                .ok_or(AppError::ProtocolDefinition)?;
            if x.name() != y.name()
                || x.json_name() != y.json_name()
                || x.cardinality() != y.cardinality()
                || x.is_map() != y.is_map()
                || x.is_packed() != y.is_packed()
                || x.supports_presence() != y.supports_presence()
                || x.field_descriptor_proto().proto3_optional()
                    != y.field_descriptor_proto().proto3_optional()
                || x.containing_oneof().map(|o| o.name().to_owned())
                    != y.containing_oneof().map(|o| o.name().to_owned())
                || x.field_descriptor_proto().default_value
                    != y.field_descriptor_proto().default_value
            {
                return invalid();
            }
            match (x.kind(), y.kind()) {
                (Kind::Message(m), Kind::Message(n)) if m.full_name() == n.full_name() => {
                    pending.push(m)
                }
                (Kind::Enum(m), Kind::Enum(n)) if m.full_name() == n.full_name() => {
                    if m.default_value().number() != n.default_value().number()
                        || m.default_value().name() != n.default_value().name()
                    {
                        return invalid();
                    }
                    for value in m.values() {
                        if n.get_value_by_name(value.name())
                            .is_none_or(|v| v.number() != value.number())
                            || n.get_value(value.number()).map(|v| v.name().to_owned())
                                != m.get_value(value.number()).map(|v| v.name().to_owned())
                        {
                            return invalid();
                        }
                    }
                }
                (m, n) if m == n => {}
                _ => return invalid(),
            }
        }
    }
    Ok(())
}
