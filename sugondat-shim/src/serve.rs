use crate::{
    adapters::sovereign::SovereignAdapter, cli::AdapterServerParams, sugondat_rpc::Client,
};
use clap::Args;
use jsonrpsee::{server::Server, Methods};
use sugondat_shim_common_sovereign::SovereignRPCServer;
use tracing::{debug, info};

#[derive(Debug, Args)]
pub struct Params {
    /// The address of the sugondat-node to connect to.
    #[clap(long, default_value = "ws://localhost:9944")]
    node_url: String,

    #[clap(flatten)]
    adapter: AdapterServerParams,
}

pub async fn run(params: Params) -> anyhow::Result<()> {
    info!(
        "starting sugondat-shim server on {}:{}",
        params.adapter.address, params.adapter.port
    );
    let listen_on = (params.adapter.address.as_str(), params.adapter.port);
    let server = Server::builder().build(listen_on).await?;
    let client = connect_client(&params.node_url).await?;
    let handle = server.start(init_adapters(client, &params.adapter));
    handle.stopped().await;
    Ok(())
}

async fn connect_client(url: &str) -> anyhow::Result<Client> {
    let client = Client::new(url.to_string()).await?;
    Ok(client)
}

fn init_adapters(client: Client, adapter: &AdapterServerParams) -> Methods {
    let mut methods = Methods::new();
    if adapter.enable_sovereign() {
        debug!("enabling sovereign adapter");
        let adapter = SovereignAdapter::new(client.clone());
        methods.merge(adapter.into_rpc()).unwrap();
    }
    methods
}
