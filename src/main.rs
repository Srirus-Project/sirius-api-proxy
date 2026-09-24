use sirius_api_proxy::{api, client::GameClient, config::Config};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("sirius-api-proxy {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if !args.is_empty() && args != ["master-update"] {
        if args.len() != 3 || args[0] != "master-import" {
            return Err(
                "usage: sirius-api-proxy [master-update | master-import ENCRYPTED_DIRECTORY OUTPUT_DIRECTORY]".into(),
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
    let config: Config = yaml_serde::from_str(&std::fs::read_to_string(path)?)?;
    config.validate()?;
    let client = GameClient::new(config.clone())?;
    let updater = if config.master_update.is_some() {
        Some(sirius_api_proxy::master_update::MasterUpdater::new(
            &config,
            client.clone(),
        )?)
    } else {
        None
    };
    if args == ["master-update"] {
        let updater = updater.ok_or("master_update configuration is required")?;
        println!("{}", updater.update_once().await?);
        return Ok(());
    }
    let api_token = std::env::var(&config.api_token_env)
        .map_err(|_| "API token environment variable is missing")?;
    let internal_token = std::env::var(&config.internal_token_env)
        .map_err(|_| "internal token environment variable is missing")?;
    if api_token.trim().is_empty() || internal_token.trim().is_empty() {
        return Err("tokens must not be empty".into());
    }
    if api_token == internal_token {
        return Err("API and internal tokens must be distinct".into());
    }
    let listen = config.listen;
    let router = api::router(client, api_token, internal_token);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let worker = updater.map(|u| tokio::spawn(u.run(receiver)));
    let signal_shutdown = shutdown.clone();
    tracing::info!(%listen,"Sirius API Proxy listening");
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
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
    if let Some(worker) = worker {
        worker.await?;
    }
    result?;
    Ok(())
}
