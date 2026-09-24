use sirius_api_proxy::{client::GameClient, deployment::DeploymentConfig};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let path = if args.is_empty() || args == ["master-update"] {
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
    if matches!(
        args.first().map(String::as_str),
        Some("asset-dispatch-status" | "asset-dispatch-adopt")
    ) {
        let adopt = args[0] == "asset-dispatch-adopt";
        if args.len() != if adopt { 4 } else { 2 } {
            return Err("usage: sirius-api-proxy asset-dispatch-status STATE_DIRECTORY | asset-dispatch-adopt STATE_DIRECTORY DISPATCH_KEY JOB_UUID".into());
        }
        let directory = std::path::Path::new(&args[1]);
        if !directory.join("outbox.json").is_file() {
            return Err("existing asset dispatch state is required".into());
        }
        // Exclusive ownership requires stopping the API worker before offline recovery.
        let mut outbox = sirius_api_proxy::asset_outbox::Outbox::open(directory, 100_000)?;
        if adopt {
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
    if !args.is_empty() && args != ["master-update"] {
        if args.len() != 3 || args[0] != "master-import" {
            return Err(
                "usage: sirius-api-proxy [master-update | master-import ENCRYPTED_DIRECTORY OUTPUT_DIRECTORY | asset-dispatch-status STATE_DIRECTORY | asset-dispatch-adopt STATE_DIRECTORY DISPATCH_KEY JOB_UUID]".into(),
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
    if args == ["master-update"] {
        let config = deployment.single()?;
        let client = GameClient::new(config.clone())?;
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
            .asset_dispatchers
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
