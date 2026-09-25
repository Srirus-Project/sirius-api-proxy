use sirius_api_proxy::{client::GameClient, deployment::DeploymentConfig};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "registry-serve") {
        if args.len() != 2 {
            return Err("usage: sirius-api-proxy registry-serve REGISTRY_CONFIG".into());
        }
        let config =
            sirius_api_proxy::registry_service::Config::load(std::path::Path::new(&args[1]))?;
        let _logging = config.logging.clone().unwrap_or_default().init()?;
        let prepared = config.prepare()?;
        let listener = tokio::net::TcpListener::bind(prepared.listen).await?;
        tracing::info!(listen=%prepared.listen,"Sirius Master registry listening");
        sirius_api_proxy::server::serve(listener, prepared.router, prepared.tls, async {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("signal handler");
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await?;
        return Ok(());
    }
    let path = if args.is_empty() || (args == ["master-update"] || args == ["master-sync"]) {
        Some(std::path::PathBuf::from(
            std::env::var("SIRIUS_CONFIG_PATH").unwrap_or_else(|_| "sirius-api-config.yaml".into()),
        ))
    } else {
        None
    };
    let _logging = sirius_api_proxy::application_log::Config::from_file(path.as_deref())?.init()?;
    let result = run().await;
    if result.is_err() {
        tracing::error!(
            error_code = "operation_failed",
            "Sirius API Proxy stopped with an error"
        );
    }
    result
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("sirius-api-proxy {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "master-db-import" | "master-db-migrate"))
    {
        if args.len() != 2 {
            return Err(
                "usage: sirius-api-proxy master-db-import|master-db-migrate DATABASE_CONFIG".into(),
            );
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&args[1])
            .map_err(|_| "database configuration unavailable")?
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| "database configuration unavailable")?;
        if bytes.len() > 65536 {
            return Err("database configuration too large".into());
        }
        let import: sirius_api_proxy::master_database::Import =
            yaml_serde::from_slice(&bytes).map_err(|_| "invalid database configuration")?;
        let receipt = if args[0] == "master-db-migrate" {
            serde_json::to_value(
                sirius_api_proxy::master_database::migrate_history(
                    &import.database,
                    &import.source,
                    import.scope,
                )
                .await?,
            )?
        } else {
            serde_json::to_value(
                sirius_api_proxy::master_database::publish(
                    &import.database,
                    &import.source,
                    import.scope,
                )
                .await?,
            )?
        };
        println!("{}", serde_json::to_string(&receipt)?);
        return Ok(());
    }
    if matches!(
        args.first().map(String::as_str),
        Some(
            "asset-dispatch-status"
                | "asset-dispatch-adopt"
                | "asset-dispatch-archive"
                | "asset-dispatch-entry"
        )
    ) {
        let adopt = args[0] == "asset-dispatch-adopt";
        let archive = args[0] == "asset-dispatch-archive";
        let entry = args[0] == "asset-dispatch-entry";
        if args.len()
            != if adopt || archive {
                4
            } else if entry {
                3
            } else {
                2
            }
        {
            return Err("usage: sirius-api-proxy asset-dispatch-status STATE_DIRECTORY | asset-dispatch-adopt STATE_DIRECTORY DISPATCH_KEY JOB_UUID | asset-dispatch-archive STATE_DIRECTORY DISPATCH_KEY JOB_UUID | asset-dispatch-entry STATE_DIRECTORY DISPATCH_KEY".into());
        }
        let directory = std::path::Path::new(&args[1]);
        if !directory.join("outbox.json").is_file() {
            return Err("existing asset dispatch state is required".into());
        }
        // Exclusive ownership requires stopping the API worker before offline recovery.
        let mut outbox = sirius_api_proxy::asset_outbox::Outbox::open(directory, 100_000)?;
        if archive {
            println!(
                "{}",
                serde_json::to_string(&outbox.archive_completed(&args[2], &args[3])?)?
            );
        } else if entry {
            let value = outbox
                .entries()
                .get(&args[2])
                .cloned()
                .or(outbox.archived(&args[2])?);
            println!("{}", serde_json::to_string(&value)?);
        } else if adopt {
            outbox.adopt(&args[2], &args[3])?;
            println!(
                "{}",
                serde_json::to_string(&outbox.entries().get(&args[2]))?
            );
        } else {
            println!("{}", serde_json::to_string(outbox.entries())?);
        }
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "master-git-commit" | "master-git-push"))
    {
        let push = args[0] == "master-git-push";
        if args.len() != if push { 3 } else { 2 } {
            return Err("usage: sirius-api-proxy master-git-commit GIT_STATE_DIRECTORY | master-git-push GIT_STATE_DIRECTORY REMOTE_URL".into());
        }
        let path =
            std::env::var("SIRIUS_CONFIG_PATH").unwrap_or_else(|_| "sirius-api-config.yaml".into());
        let deployment = DeploymentConfig::parse(&std::fs::read_to_string(path)?)?;
        let config = deployment.single()?;
        let source = config
            .master_directory
            .as_deref()
            .ok_or("master_directory is required")?;
        let scope = sirius_api_proxy::master_registry::Scope {
            region: config.region,
            environment: config.environment.clone(),
            platform: config.platform(),
        };
        let policy = config
            .master_git
            .as_ref()
            .map(|g| g.commit.clone())
            .unwrap_or_default();
        let receipt = if push {
            let remote = sirius_api_proxy::master_git::Remote {
                proxy_url_env: std::env::var_os("SIRIUS_MASTER_GIT_PROXY_URL")
                    .map(|_| "SIRIUS_MASTER_GIT_PROXY_URL".into()),
                url: args[2].clone(),
                authorization_env: std::env::var_os("SIRIUS_MASTER_GIT_AUTHORIZATION")
                    .map(|_| "SIRIUS_MASTER_GIT_AUTHORIZATION".into()),
                allow_http: false,
                allow_file: args[2].starts_with("file://"),
            };
            sirius_api_proxy::master_git::publish_with_policy(
                source,
                std::path::Path::new(&args[1]),
                scope,
                &remote,
                &policy,
            )
            .await?
        } else {
            sirius_api_proxy::master_git::commit_with_policy(
                source,
                std::path::Path::new(&args[1]),
                scope,
                &policy,
            )
            .await?
        };
        println!("{}", serde_json::to_string(&receipt)?);
        return Ok(());
    }
    if !args.is_empty() && args != ["master-update"] && args != ["master-sync"] {
        if args.len() != 3 || args[0] != "master-import" {
            return Err(
                "usage: sirius-api-proxy [master-update | master-sync | master-import ENCRYPTED_DIRECTORY OUTPUT_DIRECTORY | asset-dispatch-status STATE_DIRECTORY | asset-dispatch-adopt STATE_DIRECTORY DISPATCH_KEY JOB_UUID]".into(),
            );
        }
        use sirius_api_proxy::master::{import_directory, key_from_hex, MasterDecoder};
        let key = key_from_hex(
            &std::env::var("SIRIUS_MASTER_KEY_HEX")
                .map_err(|_| "SIRIUS_MASTER_KEY_HEX is missing")?,
        )?;
        let iv = key_from_hex(
            &std::env::var("SIRIUS_MASTER_IV_HEX")
                .map_err(|_| "SIRIUS_MASTER_IV_HEX is missing")?,
        )?;
        let receipt = import_directory(
            std::path::Path::new(&args[1]),
            std::path::Path::new(&args[2]),
            &MasterDecoder::new(&key, iv),
        )
        .map_err(|error| error.to_string())?;
        println!("{}", serde_json::to_string(&receipt)?);
        return Ok(());
    }
    let path =
        std::env::var("SIRIUS_CONFIG_PATH").unwrap_or_else(|_| "sirius-api-config.yaml".into());
    let deployment = DeploymentConfig::parse(&std::fs::read_to_string(path)?)?;
    if args == ["master-update"] || args == ["master-sync"] {
        let config = deployment.single()?;
        let client = GameClient::new(config.clone())?;
        if args == ["master-sync"] {
            let syncer = sirius_api_proxy::master_sync::Syncer::new(config, client)?;
            println!("{}", syncer.update_once().await?);
            return Ok(());
        }
        if config.master_update.is_none() {
            return Err("master_update configuration is required".into());
        }
        let updater = sirius_api_proxy::master_update::MasterUpdater::new(config, client)?;
        println!("{}", updater.update_once().await?);
        return Ok(());
    }
    let prepared = deployment.prepare()?;
    let listen = prepared.listen;
    let router = prepared.router;
    let tls = prepared.tls;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let mut workers: Vec<_> = prepared
        .updaters
        .into_iter()
        .map(|u| tokio::spawn(u.run(receiver.clone())))
        .collect();
    workers.extend(
        prepared
            .syncers
            .into_iter()
            .map(|syncer| tokio::spawn(syncer.run(receiver.clone()))),
    );
    workers.extend(
        prepared
            .asset_dispatchers
            .into_iter()
            .map(|worker| tokio::spawn(worker.run(receiver.clone()))),
    );
    workers.extend(
        prepared
            .notifiers
            .into_iter()
            .map(|worker| tokio::spawn(worker.run(receiver.clone()))),
    );
    workers.extend(
        prepared
            .git_publishers
            .into_iter()
            .map(|worker| tokio::spawn(worker.run(receiver.clone()))),
    );
    workers.extend(
        prepared
            .database_publishers
            .into_iter()
            .map(|worker| tokio::spawn(worker.run(receiver.clone()))),
    );
    let signal_shutdown = shutdown.clone();
    tracing::info!(%listen,"Sirius API Proxy listening");
    let result = sirius_api_proxy::server::serve(listener, router, tls, async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = signal_shutdown.send(true);
    })
    .await;
    let _ = shutdown.send(true);
    for worker in workers {
        worker.await?;
    }
    result?;
    Ok(())
}
