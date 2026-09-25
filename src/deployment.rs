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
    #[serde(default)]
    pub logging: Option<crate::application_log::Config>,
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
    pub git_publishers: Vec<crate::master_git_worker::Worker>,
    pub notifiers: Vec<crate::master_notify::Worker>,
    pub syncers: Vec<Arc<crate::master_sync::Syncer>>,
    pub asset_dispatchers: Vec<crate::asset_dispatch::Worker>,
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
                "Master commands require a single-region configuration",
            )),
        }
    }

    pub fn validate(&self) -> Result<(), AppError> {
        match self {
            Self::Single(c) => c.validate(),
            Self::Multi(m) => {
                if let Some(log) = &m.logging {
                    log.validate().map_err(|_| {
                        AppError::Config("invalid application logging configuration")
                    })?;
                }
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
                    if c.listen.is_some()
                        || c.tls.is_some()
                        || c.access_log.is_some()
                        || c.logging.is_some()
                    {
                        return Err(AppError::Config(
                            "listen, tls, logging and access_log belong at the deployment root, not inside regions",
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
        let peer_tokens = configs
            .iter()
            .map(|c| {
                c.peer_token_env
                    .as_ref()
                    .map(|name| secret(name))
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (index, token) in peer_tokens
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.as_ref().map(|t| (i, t)))
        {
            if token.bytes().any(|b| b.is_ascii_whitespace())
                || tokens
                    .iter()
                    .any(|(public, internal)| token == public || token == internal)
                || peer_tokens
                    .iter()
                    .enumerate()
                    .any(|(i, other)| i != index && other.as_ref() == Some(token))
                || configs.iter().any(|c| {
                    c.cdn_credential_env
                        .values()
                        .chain(c.player_credential_env.iter())
                        .chain(c.accounts.iter().filter_map(|a| a.credential_env.as_ref()))
                        .any(|name| std::env::var(name).is_ok_and(|value| value == *token))
                })
            {
                return Err(AppError::Config("peer tokens must be region-scoped and distinct from API/internal/game/CDN credentials").into());
            }
        }
        for c in &configs {
            if let Some(dispatch) = &c.asset_dispatch {
                for target in &dispatch.targets {
                    let token = secret(&target.token_env)?;
                    if peer_tokens.iter().flatten().any(|peer| peer == &token)
                        || tokens
                            .iter()
                            .any(|(public, internal)| token == *public || token == *internal)
                        || configs.iter().any(|config| {
                            config
                                .cdn_credential_env
                                .values()
                                .any(|name| std::env::var(name).is_ok_and(|value| value == token))
                        })
                    {
                        return Err(AppError::Config("asset updater token must be distinct from API/internal/CDN credentials").into());
                    }
                }
            }
        }
        let mut outgoing = Vec::new();
        for c in &configs {
            if let Some(routing) = &c.node_routing {
                for target in &routing.targets {
                    let token = secret(&target.token_env)?;
                    if tokens
                        .iter()
                        .any(|(public, internal)| token == *public || token == *internal)
                        || configs.iter().zip(&peer_tokens).any(|(other, peer)| {
                            other.region != c.region && peer.as_ref() == Some(&token)
                        })
                        || outgoing
                            .iter()
                            .any(|(region, value)| *region != c.region && value == &token)
                        || configs.iter().any(|other| {
                            other
                                .cdn_credential_env
                                .values()
                                .chain(other.player_credential_env.iter())
                                .chain(
                                    other
                                        .accounts
                                        .iter()
                                        .filter_map(|a| a.credential_env.as_ref()),
                                )
                                .chain(
                                    other
                                        .asset_dispatch
                                        .iter()
                                        .flat_map(|d| d.targets.iter().map(|t| &t.token_env)),
                                )
                                .any(|name| std::env::var(name).is_ok_and(|value| value == token))
                        })
                    {
                        return Err(AppError::Config("outgoing peer tokens must be region scoped and distinct from administrative/game/CDN/updater credentials").into());
                    }
                    outgoing.push((c.region, token));
                }
            }
        }
        for c in &configs {
            if let Some(sync) = &c.master_sync {
                let token = secret(&sync.token_env)?;
                if tokens.iter().any(|(_, internal)| token == *internal)
                    || peer_tokens.iter().flatten().any(|peer| peer == &token)
                    || outgoing.iter().any(|(_, peer)| peer == &token)
                    || configs.iter().any(|other| {
                        other
                            .cdn_credential_env
                            .values()
                            .chain(other.player_credential_env.iter())
                            .chain(
                                other
                                    .accounts
                                    .iter()
                                    .filter_map(|a| a.credential_env.as_ref()),
                            )
                            .chain(
                                other
                                    .asset_dispatch
                                    .iter()
                                    .flat_map(|d| d.targets.iter().map(|t| &t.token_env)),
                            )
                            .any(|name| std::env::var(name).is_ok_and(|value| value == token))
                    })
                {
                    return Err(AppError::Config("Master owner read token must be distinct from administrative/peer/game/CDN/updater credentials").into());
                }
            }
        }
        crate::master_notify::validate_tokens(&configs)?;
        crate::master_git_worker::validate_tokens(&configs)?;
        let mut git_publishers = Vec::new();
        let mut notifiers = Vec::new();
        let mut router = api::health_router();
        let mut updaters = Vec::new();
        let mut syncers = Vec::new();
        let mut asset_dispatchers = Vec::new();
        for ((c, (public, internal)), peer_token) in
            configs.into_iter().zip(tokens).zip(peer_tokens)
        {
            let client = GameClient::new(c.clone())?;
            let dispatcher = if c.asset_dispatch.is_some() {
                Some(crate::asset_dispatch::Worker::new(c, client.clone())?)
            } else {
                None
            };
            if c.master_git.is_some() {
                git_publishers.push(crate::master_git_worker::Worker::new(c, client.clone())?);
            }
            if c.master_notify.is_some() {
                notifiers.push(crate::master_notify::Worker::new(c, client.clone())?);
            }
            if c.master_sync.is_some() {
                syncers.push(crate::master_sync::Syncer::new(c, client.clone())?);
            }
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
            if let Some(worker) = dispatcher {
                router = router.merge(crate::asset_dispatch_admin::router(
                    worker.control(),
                    &format!("{internal_prefix}/asset-dispatch"),
                    internal.clone(),
                ));
                asset_dispatchers.push(worker);
            }
            if let Some(token) = peer_token {
                router = router.merge(crate::peer::router(
                    client.clone(),
                    &format!("{internal_prefix}/peer"),
                    token,
                ));
            }
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
            syncers,
            notifiers,
            git_publishers,
            asset_dispatchers,
        })
    }
}
