//! Service assembly keeps each region's protocol, account and observations independent.
use crate::{
    api,
    client::GameClient,
    config::{secret, Config},
    error::AppError,
    master_update::MasterUpdater,
};
use axum::Router;
use serde::Deserialize;
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiConfig {
    pub listen: SocketAddr,
    #[serde(default)]
    pub tls: Option<crate::server::TlsConfig>,
    #[serde(default)]
    pub access_log: Option<crate::access_log::Config>,
    pub regions: BTreeMap<String, Config>,
}

pub enum DeploymentConfig {
    Single(Box<Config>),
    Multi(Box<MultiConfig>),
}

pub struct Prepared {
    pub tls: Option<crate::server::LoadedTls>,
    pub listen: SocketAddr,
    pub router: Router,
    pub updaters: Vec<Arc<MasterUpdater>>,
}

impl DeploymentConfig {
    pub fn parse(text: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // Select explicitly so a malformed multi-region file never falls back to JP.
        let value: yaml_serde::Value = yaml_serde::from_str(text)?;
        let config = if value.get("regions").is_some() {
            Self::Multi(yaml_serde::from_value(value)?)
        } else {
            Self::Single(yaml_serde::from_value(value)?)
        };
        config.validate()?;
        Ok(config)
    }

    pub fn single(&self) -> Result<&Config, AppError> {
        match self {
            Self::Single(c) => Ok(c),
            Self::Multi(_) => Err(AppError::Config(
                "master-update requires a single-region configuration",
            )),
        }
    }

    pub fn validate(&self) -> Result<(), AppError> {
        match self {
            Self::Single(c) => c.validate(),
            Self::Multi(m) => {
                if let Some(tls) = &m.tls {
                    tls.validate()
                        .map_err(|_| AppError::Config("invalid listener TLS configuration"))?;
                }
                if let Some(log) = &m.access_log {
                    log.validate()
                        .map_err(|_| AppError::Config("invalid access log configuration"))?;
                }
                if m.regions.is_empty() || m.regions.len() > 4 {
                    return Err(AppError::Config(
                        "configure one to four operational regions",
                    ));
                }
                for (name, c) in &m.regions {
                    if name != c.region.name() {
                        return Err(AppError::Config(
                            "region map key must equal the explicit region identity",
                        ));
                    }
                    if c.listen.is_some() || c.tls.is_some() || c.access_log.is_some() {
                        return Err(AppError::Config(
                            "listen, tls and access_log belong at the deployment root, not inside regions",
                        ));
                    }
                    c.validate()?;
                }
                Ok(())
            }
        }
    }

    pub fn prepare(&self) -> Result<Prepared, Box<dyn std::error::Error>> {
        self.validate()?;
        let tls = match self {
            Self::Single(c) => c.tls.as_ref(),
            Self::Multi(m) => m.tls.as_ref(),
        }
        .map(crate::server::TlsConfig::load)
        .transpose()?;
        let (listen, configs, regional) = match self {
            Self::Single(c) => (
                c.listen
                    .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 9999))),
                vec![c.as_ref()],
                false,
            ),
            Self::Multi(m) => (m.listen, m.regions.values().collect(), true),
        };
        let mut tokens = Vec::new();
        for c in &configs {
            let public = secret(&c.api_token_env)?;
            let internal = secret(&c.internal_token_env)?;
            if public.trim().is_empty() || internal.trim().is_empty() {
                return Err(AppError::Config("tokens must not be blank").into());
            }
            tokens.push((public, internal));
        }
        // An API bearer must not acquire internal privileges in any configured region.
        if tokens
            .iter()
            .any(|(p, _)| tokens.iter().any(|(_, i)| p == i))
        {
            return Err(AppError::Config(
                "API and internal tokens must be distinct across all regions",
            )
            .into());
        }
        let mut router = api::health_router();
        let mut updaters = Vec::new();
        for (c, (public, internal)) in configs.into_iter().zip(tokens) {
            let client = GameClient::new(c.clone())?;
            if c.master_update.is_some() {
                updaters.push(MasterUpdater::new(c, client.clone())?);
            }
            let (api_prefix, internal_prefix) = if regional {
                (
                    format!("/api/v1/{}", c.region.name()),
                    format!("/internal/v1/{}", c.region.name()),
                )
            } else {
                ("/api/v1".into(), "/internal/v1".into())
            };
            router = router.merge(api::router_at(
                client,
                public,
                internal,
                &api_prefix,
                &internal_prefix,
            ));
        }
        let access = match self {
            Self::Single(c) => c.access_log.as_ref(),
            Self::Multi(m) => m.access_log.as_ref(),
        };
        if let Some(config) = access {
            router = crate::access_log::AccessLog::new(config.clone())?.wrap(router);
        }
        Ok(Prepared {
            tls,
            listen,
            router,
            updaters,
        })
    }
}
